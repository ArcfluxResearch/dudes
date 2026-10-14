//! What clients see. The floor's store *is* a [`FloorProjection`], fed by the initial
//! `floor.subscribe` result and then by [`FloorDelta`] notifications. It holds nothing the
//! journal does not have, apart from activity lifetime metadata derived from its recorded time
//! and the daemon's clock; `floor.resync` throws it away and starts over.

use dudes_types::{
    AgentId, AgentStatus, BreakerRung, CardId, CardStatus, GoalId, GoalStatus, Lsn, MailId,
    MailState, Mode, Provenance, Provider, Seq, SessionId, Timestamp,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::budget::{GoalBudget, Spend};
use crate::record::{Actor, ClosingPhase, MailAct, MailFrom, MailTo, RecordedWorkspace};

/// The whole floor at one `lsn`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct FloorProjection {
    /// The journal position this projection reflects.
    pub lsn: Lsn,
    /// Every hired, unretired agent.
    pub agents: Vec<AgentView>,
    /// Every card on the board.
    pub cards: Vec<CardView>,
    /// Recent mail, newest last.
    pub mail: Vec<MailView>,
    /// Recent permitted journal events in ascending journal order.
    pub activity: Vec<ActivityView>,
    /// Retired agents in retirement journal order, within the archive limits.
    pub retired_agents: Vec<RetiredAgentView>,
    /// The closing-time step in progress, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closing_time: Option<ClosingPhase>,
}

/// What changed since the previous delta. Each entity present is its complete current view;
/// `removed_agents` names agents that were retired.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct FloorDelta {
    /// The journal position after applying this delta.
    pub lsn: Lsn,
    /// Agents whose view changed.
    pub agents: Vec<AgentView>,
    /// Cards whose view changed.
    pub cards: Vec<CardView>,
    /// Mail whose view changed or that is new.
    pub mail: Vec<MailView>,
    /// Complete recent activity window when it changed, including an empty replacement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<Vec<ActivityView>>,
    /// Complete retired-agent archive when changed, including an empty replacement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_agents: Option<Vec<RetiredAgentView>>,
    /// Agents retired since the previous delta.
    pub removed_agents: Vec<AgentId>,
    /// Cards removed from the board since the previous delta. An older reader ignores it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed_cards: Vec<CardId>,
    /// The closing-time step, when it changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closing_time: Option<ClosingPhase>,
}

/// Browser snapshot with optional aggregate closing progress.
#[derive(Clone, Copy, Debug, Serialize, JsonSchema)]
pub struct FloorProjectionView<'a> {
    /// The existing projection at the same durable cut.
    #[serde(flatten)]
    pub projection: &'a FloorProjection,
    /// Aggregate progress while this run is closing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closing_progress: Option<ClosingProgress>,
}

/// Browser delta with optional aggregate closing progress.
#[derive(Clone, Copy, Debug, Serialize, JsonSchema)]
pub struct FloorDeltaView<'a> {
    /// The existing changes at the same durable cut.
    #[serde(flatten)]
    pub delta: &'a FloorDelta,
    /// Aggregate progress when counts or the phase changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closing_progress: Option<ClosingProgress>,
}

/// Read-only closing progress without participant identities.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClosingProgress {
    /// Participants with a durable acknowledgement.
    pub acknowledged: u32,
    /// Unacknowledged participants with nothing left to save.
    pub settled: u32,
    /// Participants that still owe an acknowledgement.
    pub owed: u32,
    /// Participants with at least one turn awaiting an Observed end.
    pub running: u32,
    /// Earlier monotonic or wall time remaining, in milliseconds.
    pub remaining_ms: u64,
}

/// Where a worktree is in its life, as the Floor and `worktree.list` show it.
///
/// Derived from the journal. The Floor's copy cannot see an agent's recorded children, so it reads
/// `released` the moment retirement is recorded; `worktree.list` is exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeViewState {
    /// Leased, and no agent hired into it yet.
    Pending,
    /// Its agent is live.
    Active,
    /// Its agent has retired and nothing has removed the tree.
    Released,
    /// The operator removed it, or it was never assigned.
    Removed,
}

/// The facts the Floor may show of an agent's worktree. Never a path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeView {
    /// The branch the tree was created on.
    pub branch: String,
    /// Where the lease is in its life.
    pub state: WorktreeViewState,
    /// The lease this one was based on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<dudes_types::WorktreeId>,
}

/// One agent as the floor shows it. Status is projected from `TurnIssued` / `TurnEnded` /
/// `HumanQuestion`, never from PTY bytes or silence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentView {
    /// The agent.
    pub id: AgentId,
    /// Display name.
    pub name: String,
    /// Authored cast character. Absence preserves the automatic appearance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character: Option<dudes_types::CastCharacter>,
    /// Named accent colour. Absence preserves the neutral accent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<dudes_types::AccentColour>,
    /// Role text.
    pub role: String,
    /// Journal position of the newest appearance edit, as decimal text. Absent before the first edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub look_lsn: Option<String>,
    /// Department.
    pub department: String,
    /// Which CLI drives it.
    pub provider: Provider,
    /// Workspace identity and label recorded at hire, never a filesystem path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<RecordedWorkspace>,
    /// Where it is in its life.
    pub status: AgentStatus,
    /// Latest content-free, recent observed activity. Absence makes no current claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_activity: Option<LiveActivityView>,
    /// Managed, or attached to a human.
    pub mode: Mode,
    /// `loop.pause` is standing.
    #[serde(default)]
    pub paused: bool,
    /// `loop.halt` is standing.
    #[serde(default)]
    pub halted: bool,
    /// An automatic hold requiring Resume or a new goal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automatic_hold: Option<crate::AutomaticHold>,
    /// Current breaker rung.
    #[serde(default = "healthy")]
    pub breaker: BreakerRung,
    /// The goal it pursues, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalView>,
    /// Managed turns issued over the agent's life.
    #[serde(default)]
    pub turns: u64,
    /// Spend today, against the agent's daily budget.
    #[serde(default)]
    pub spent_today: Spend,
    /// Unacknowledged mail in its inbox.
    #[serde(default)]
    pub unread: u32,
    /// The open question, while `Waiting`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// The open question's identity: the canonical decimal of the position of the
    /// `HumanQuestion` that opened it. A string, so a JavaScript number can never round it.
    /// Set only by that record; absent while no question is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_lsn: Option<String>,
    /// The agent's control identity: the canonical decimal of the position of its newest `Pause`,
    /// `Resume`, `Halt` or `AgentHeld`. A string, so a JavaScript number can never round it. Set only by those
    /// records; absent while none has been journaled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_lsn: Option<String>,
    /// The vendor-confirmed session for this agent: the id the vendor named back to us, and the
    /// only one a later turn may resume. `None` until the vendor confirms one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// When the last turn ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_ended_at: Option<Timestamp>,
    /// The agent's own git worktree, when it was hired into one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeView>,
    /// Another live agent records this agent's working directory too. A display hint computed from
    /// recorded directory equality, not a guard.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shares_checkout: bool,
}

/// One fixed kind observed by the daemon, without vendor text or tool detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LiveActivityKind {
    /// A turn or prompt is active. No claim about model thought is made.
    Working,
    /// An admitted tool boundary was reached.
    UsingTool,
    /// The provider began context compaction.
    Compacting,
    /// An open human question is waiting for its answer.
    Waiting,
    /// A turn finished.
    Idle,
}

/// The latest fixed activity kind and its journal entry time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LiveActivityView {
    /// What was observed.
    pub kind: LiveActivityKind,
    /// The journal entry timestamp in Unix milliseconds.
    pub at: Timestamp,
    /// Remaining display lifetime when the daemon produced or refreshed this value, in milliseconds.
    pub expires_in_ms: u32,
}

const fn healthy() -> BreakerRung {
    BreakerRung::Healthy
}

/// A goal as the floor shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GoalView {
    /// The goal.
    pub id: GoalId,
    /// Its text.
    pub text: String,
    /// Where it is in its life.
    pub status: GoalStatus,
    /// Its ceiling.
    pub budget: GoalBudget,
    /// Managed turns issued for it so far (`turns_on_goal`, gate 6).
    #[serde(default)]
    pub turns: u32,
    /// Its hop cap.
    pub hop_cap: u32,
    /// Spend against it so far.
    #[serde(default)]
    pub spent: Spend,
    /// The recorded configured check summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_check: Option<crate::goal::CompletionCheckRef>,
    /// Whether the check failed before the first turn or may have passed before it; absent when no
    /// baseline was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<crate::goal::CheckBaselineKind>,
    /// The latest predicate result, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_result: Option<PredicateOutcome>,
}

/// The latest `PredicateResult` for a goal, summarised.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PredicateOutcome {
    /// Whether it passed.
    pub pass: bool,
    /// Whether the daemon observed it. Only `Observed` + `pass` is `Done`.
    pub provenance: Provenance,
    /// Where the record is.
    pub lsn: Lsn,
    /// The check's program did not start, so this result judged nothing. Absent when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_run: bool,
}

/// A board card as the floor shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CardView {
    /// The card.
    pub id: CardId,
    /// Title.
    pub title: String,
    /// Department.
    pub department: String,
    /// Status.
    pub status: CardStatus,
    /// Who authored [`Self::status`]. The board fold takes it from whichever record last moved
    /// the card, so the three records that can put a card into a status collapse into one field
    /// a reader compares once. Not [`Self::claimed_by`], which is who *holds* the card.
    /// What a `BoardState` predicate does with it is decided where that predicate is evaluated, not restated
    /// here.
    pub status_by: Actor,
    /// Current sequence number; pass it as `expected_seq` when moving the card.
    pub seq: Seq,
    /// Who holds it, from `Claimed` on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<AgentId>,
    /// Cards that must be `Done` first.
    #[serde(default)]
    pub deps: Vec<CardId>,
    /// What the board shows of a ticket beyond what every card has. Absent on a plain card. An
    /// older reader ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<CardTicket>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(count: &u32) -> bool {
    *count == 0
}

/// What the floor snapshot says of a ticket. It carries no note, message, summary, path, argv,
/// check name or check output: those come only from `ticket.read`. It is small on purpose, because
/// the snapshot's card budget has to hold the most tickets the board allows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CardTicket {
    /// Its place in its holder's queue, counting from 1, while it waits there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    /// How many times work on the ticket has started; absent before the first start.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub round: u32,
    /// Why the daemon moved the ticket into Needs you, while it is there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<crate::ticket::TicketReason>,
    /// The ticket was stopped and its agent is still finishing its current step.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stopping: bool,
    /// Bounded action/evidence summary. Paths and interval details come from `ticket.read`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_review: Option<crate::instruction::InstructionReviewSummary>,
}

/// Legacy mail projection shape retained for construction compatibility.
/// Live Floor frames use [`crate::MessageView`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MailView {
    /// The item.
    pub id: MailId,
    /// Sender.
    pub from: MailFrom,
    /// Recipient.
    pub to: MailTo,
    /// What it asks.
    pub act: MailAct,
    /// Subject line.
    pub subject: String,
    /// Delivery state.
    pub state: MailState,
}

/// Maximum number of agents in the Floor retired-agent archive.
pub const MAX_RETIRED_AGENTS: usize = 64;
/// Maximum UTF-8 bytes of the serialized retired-agent array, including JSON escaping.
pub const MAX_RETIRED_AGENT_BYTES: usize = 32_768;

/// A retired agent's permitted display facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RetiredAgentView {
    /// The structural agent identity.
    pub id: AgentId,
    /// Display name at retirement.
    pub name: String,
    /// Role at retirement.
    pub role: String,
    /// Authored character at retirement, or automatic selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character: Option<dudes_types::CastCharacter>,
    /// Named accent at retirement, or automatic selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<dudes_types::AccentColour>,
    /// Which CLI drove it.
    pub provider: Provider,
    /// Recorded workspace label, never a path or workspace identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_label: Option<String>,
    /// Recorded hire time.
    pub hired_at: Timestamp,
    /// Recorded retirement time. Ordering follows the journal, not this value.
    pub retired_at: Timestamp,
    /// Distinct recorded turn identities across managed and attached modes.
    pub turns: u64,
    /// Total normalized reported spend across the agent's day buckets.
    pub spend: Spend,
    /// The agent's worktree and where it is now, so a released tree stays visible until the
    /// operator removes it. Filled when the archive is read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeView>,
}

/// Maximum number of events in the Floor activity window.
pub const MAX_ACTIVITY_ROWS: usize = 64;
/// Maximum UTF-8 bytes of the serialized activity array, including JSON escaping.
pub const MAX_ACTIVITY_BYTES: usize = 32_768;

/// One permitted journal event. Its position is decimal text to preserve every `u64` in browsers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActivityView {
    /// Canonical decimal journal position.
    pub lsn: String,
    /// The journal entry's recorded time. Ordering depends only on `lsn`.
    pub at: Timestamp,
    /// Explicit display facts, without the record's private payload.
    #[serde(flatten)]
    pub event: ActivityEvent,
}

/// An agent identity and its display name at the event's journal position.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActivityAgent {
    /// Journaled identity.
    pub id: AgentId,
    /// Recorded display name, absent when the journal names no hired agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A card's display facts at the event's journal position.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActivityCard {
    /// Journaled identity.
    pub id: CardId,
    /// Recorded title.
    pub title: String,
    /// The status created or reached by this event.
    pub status: CardStatus,
}

/// The complete activity allowlist. No generic record or text fallback crosses this boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityEvent {
    /// A managed turn was issued.
    TurnIssued {
        /// The owner.
        agent: ActivityAgent,
    },
    /// A turn ended.
    TurnEnded {
        /// The owner.
        agent: ActivityAgent,
    },
    /// An agent received a goal.
    GoalSet {
        /// The owner.
        agent: ActivityAgent,
    },
    /// The loop observed the goal as done.
    GoalDone {
        /// The owner.
        agent: ActivityAgent,
    },
    /// A board card was created.
    CardCreated {
        /// The created card.
        card: ActivityCard,
    },
    /// An agent claimed a board card.
    CardClaimed {
        /// The card after the claim.
        card: ActivityCard,
        /// The claimer.
        agent: ActivityAgent,
    },
    /// A board card changed status.
    CardTransition {
        /// The card after the move.
        card: ActivityCard,
    },
    /// An agent opened a question.
    HumanQuestion {
        /// The asker.
        agent: ActivityAgent,
    },
    /// A human answered an agent's question.
    HumanAnswer {
        /// The recipient.
        agent: ActivityAgent,
    },
    /// An agent retired.
    AgentRetired {
        /// The retired agent.
        agent: ActivityAgent,
    },
    /// An agent was paused.
    Pause {
        /// The agent.
        agent: ActivityAgent,
    },
    /// An agent was resumed.
    Resume {
        /// The agent.
        agent: ActivityAgent,
    },
    /// An agent received new display text.
    AgentRenamed {
        /// The agent with its new name recorded at this event.
        agent: ActivityAgent,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The floor is a view: an empty projection is valid and every list defaults, so a client
    /// can start from `{}` and apply deltas.
    #[test]
    fn empty_projection_is_valid() {
        let projection: FloorProjection = serde_json::from_str("{}").unwrap();
        assert_eq!(projection, FloorProjection::default());
        let delta: FloorDelta = serde_json::from_str(r#"{"lsn": 5}"#).unwrap();
        assert_eq!(delta.lsn, Lsn(5));
        assert!(delta.agents.is_empty());
    }

    /// A reader from before the board names none of the new members, and ignores each of them: a
    /// card's `ticket` and a delta's `removed_cards`.
    #[test]
    fn an_older_reader_ignores_the_ticket_members() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct OlderCard {
            id: CardId,
            title: String,
            department: String,
            status: CardStatus,
            status_by: Actor,
            seq: Seq,
            claimed_by: Option<AgentId>,
            deps: Vec<CardId>,
        }
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct OlderDelta {
            lsn: Lsn,
            agents: Vec<AgentView>,
            cards: Vec<OlderCard>,
            removed_agents: Vec<AgentId>,
        }
        let card = CardView {
            id: CardId::from_parts(1, 1),
            title: "Ticket".to_owned(),
            department: "engineering".to_owned(),
            status: CardStatus::Claimed,
            status_by: Actor::System,
            seq: Seq(2),
            claimed_by: Some(AgentId::from_parts(1, 2)),
            deps: Vec::new(),
            ticket: Some(CardTicket {
                position: Some(2),
                round: 1,
                reason: None,
                stopping: false,
                instruction_review: None,
            }),
        };
        let delta = FloorDelta {
            cards: vec![card],
            removed_cards: vec![CardId::from_parts(1, 3)],
            ..FloorDelta::default()
        };
        let wire = serde_json::to_value(&delta).unwrap();
        assert!(wire.get("removed_cards").is_some());
        let older: OlderDelta = serde_json::from_value(wire).unwrap();
        assert_eq!(older.cards.len(), 1);
        assert_eq!(older.cards[0].title, "Ticket");
    }

    /// A result that ran keeps today's bytes, and older bytes read as a result that ran.
    #[test]
    fn a_result_names_not_run_only_when_its_check_did_not_run() {
        let ran = PredicateOutcome {
            pass: false,
            provenance: Provenance::Observed,
            lsn: Lsn(7),
            not_run: false,
        };
        let before = serde_json::json!({"pass": false, "provenance": "observed", "lsn": 7});
        assert_eq!(serde_json::to_value(ran).unwrap(), before);
        assert_eq!(
            serde_json::from_value::<PredicateOutcome>(before).unwrap(),
            ran
        );
        let not_run = PredicateOutcome {
            not_run: true,
            ..ran
        };
        let value = serde_json::to_value(not_run).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"pass": false, "provenance": "observed", "lsn": 7, "not_run": true})
        );
        assert_eq!(
            serde_json::from_value::<PredicateOutcome>(value).unwrap(),
            not_run
        );
    }

    #[test]
    fn live_activity_is_a_fixed_kind_and_recorded_time() {
        let value = serde_json::json!({
            "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "name": "Ada",
            "role": "builder",
            "department": "engineering",
            "provider": "claude",
            "status": "running",
            "mode": "managed",
            "live_activity": {"kind": "using_tool", "at": 42, "expires_in_ms": 30_000}
        });
        let agent: AgentView = serde_json::from_value(value).unwrap();
        let encoded = serde_json::to_value(agent).unwrap();
        assert_eq!(
            encoded["live_activity"],
            serde_json::json!({"kind": "using_tool", "at": 42, "expires_in_ms": 30_000})
        );
    }

    /// `status_by` is **required**, and required-ness is the property — not
    /// presence. A `CardView` that may arrive without an author is a `CardView` whose author
    /// reads as *absent means fine*, a bug class returning
    /// through the type system. So the check is that the *absence* is refused: this goes red
    /// the moment `status_by` becomes an `Option` or gains a `#[serde(default)]`, which is what
    /// makes it a check and not a restatement of the struct.
    ///
    /// It is `Actor::Peer` in the sample on purpose: this test is about the field being required,
    /// not about the check the field feeds — that check belongs to the `BoardState` predicate and is not
    /// restated.
    #[test]
    fn a_card_view_without_a_status_author_is_refused() {
        let with = serde_json::json!({
            "id": "01ARZ3NDEKTSV4RRFFQ69G5FAZ",
            "title": "Write the parser",
            "department": "engineering",
            "status": "done",
            "status_by": {"kind": "peer", "pid": 77, "uid": 501},
            "seq": 3,
            "deps": []
        });

        // Control: the same payload with the author present parses and round-trips, so the
        // failure below is about the one removed key and not about a malformed sample.
        let view: CardView =
            serde_json::from_value(with.clone()).expect("control: the sample is well formed");
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            with,
            "control: it round-trips"
        );

        let mut without = with;
        without.as_object_mut().unwrap().remove("status_by");
        let err = serde_json::from_value::<CardView>(without)
            .expect_err("a card's status must name its author");
        assert!(
            err.to_string().contains("missing field `status_by`"),
            "refused for the wrong reason: {err}"
        );
    }
}
