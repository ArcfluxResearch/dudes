//! Episode identity and immutable intent for one deliberate goal recovery.

use dudes_types::{
    AgentId, CardId, GoalId, Lsn, ModelSelection, Provider, SessionId, TurnId, WorktreeId,
};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::diff_stamp::TicketBinding;
use crate::record::Hex32;

/// Maximum supplemental recap bytes before ordinary admission packs the task.
pub const MAX_RECOVERY_RECAP_BYTES: usize = 8_192;

/// Work whose latest recovery episode is canonical, including later physical attempts.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct RecoveryWork {
    /// Unchanged managed agent.
    pub agent: AgentId,
    /// Original goal.
    pub goal: GoalId,
    /// Interrupted, failed, or still-owed turn.
    pub turn: TurnId,
}

/// The logical origin remains stable when an existing hold or pause gains a barrier.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct RecoveryEpisodeId {
    /// Exact interrupted work.
    pub work: RecoveryWork,
    /// Original boundary position, which can precede the barrier.
    pub origin: Lsn,
}

/// Recorded identities that recovery cannot redirect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryBinding {
    /// Original work.
    pub work: RecoveryWork,
    /// Exact original managed issue.
    pub issue_lsn: Lsn,
    /// Hire that owns the recorded checkout and provider configuration.
    pub hired_lsn: Lsn,
    /// Original ticket association, absent for a non-ticket goal.
    #[serde(deserialize_with = "strict_ticket")]
    #[schemars(with = "crate::chat::ExplicitNullable<RecoveryTicketFields>")]
    pub ticket: Option<TicketBinding>,
    /// Exact daemon-owned checkout lease, absent for an unleased checkout.
    #[serde(deserialize_with = "required_option")]
    #[schemars(with = "crate::chat::ExplicitNullable<WorktreeId>")]
    pub worktree: Option<WorktreeId>,
    /// Recorded provider.
    pub provider: Provider,
    /// Original explicit model choice, or an older issue with no recorded choice.
    #[serde(deserialize_with = "strict_model")]
    #[schemars(with = "crate::chat::ExplicitNullable<RecoveryModelFields>")]
    pub model_selection: Option<ModelSelection>,
}

/// How a required barrier obtains its logical origin without predicting an append position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryOrigin {
    /// The exact hold or pause already shown to the operator.
    Existing {
        /// Original control boundary.
        lsn: Lsn,
    },
    /// Resolve to the actual position only after this barrier becomes durable.
    ThisBarrier,
}

/// Context restrictions established by the recovery boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryReason {
    /// A releasable automatic hold or a paused failed turn.
    HeldOrPausedFailure,
    /// A recorded child was observed Gone and durably settled as Vanished.
    Vanished,
    /// A goal-bearing resume did not confirm its requested context.
    UnconfirmedContext,
    /// A durable issue has no recorded Spawn and requires fresh-repeat consent.
    MissingSpawn,
}

/// Exact source pair for a failure named by an automatic hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryFailure {
    /// Actual issue of the named failure, preserving its goal relationship.
    pub issue_lsn: Lsn,
    /// Actual observed terminal source for that attempt.
    pub end_lsn: Lsn,
}

/// Historical sources for the selected boundary; current liveness is a separate admission gate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryBoundary {
    /// An eligible current automatic hold and its named observed failures.
    Held {
        /// Latest turn's exact terminal source.
        end_lsn: Lsn,
        /// First failure named by a repeated-failure hold, absent for launch holds.
        #[serde(deserialize_with = "required_option")]
        #[schemars(with = "crate::chat::ExplicitNullable<RecoveryFailure>")]
        first: Option<RecoveryFailure>,
    },
    /// Current operator pause after an observed failed or no-result turn.
    Paused {
        /// Latest turn's actual observed end.
        end_lsn: Lsn,
    },
    /// Recorded child durably settled as Gone/Vanished.
    Vanished {
        /// Exact latest spawn.
        spawn_lsn: Lsn,
        /// Exact later observed Vanished end.
        end_lsn: Lsn,
    },
    /// A particular recorded-conversation attempt ended without confirming its requested context.
    Unconfirmed {
        /// Exact latest spawn.
        spawn_lsn: Lsn,
        /// Exact later qualifying observed end.
        end_lsn: Lsn,
        /// Earlier accepted confirmation of the conversation this attempt requested.
        requested_confirmation_lsn: Lsn,
    },
    /// Complete before-record rows have no spawn or terminal source for this issued obligation.
    MissingSpawn,
}

/// A new barrier and its exact predecessor when this work already has an episode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRequired {
    /// Immutable target and configuration.
    pub binding: RecoveryBinding,
    /// Existing origin or this barrier's eventual actual position.
    pub origin: RecoveryOrigin,
    /// Available context choices.
    pub reason: RecoveryReason,
    /// Exact immutable ordinary-history sources for this reason.
    pub boundary: RecoveryBoundary,
    /// Exact prior claim whose closed episode is replaced or whose issue-bound consent is superseded.
    #[serde(deserialize_with = "required_option")]
    #[schemars(with = "crate::chat::ExplicitNullable<RecoveryClaimRef>")]
    pub predecessor: Option<RecoveryClaimRef>,
}

/// An authorization is identified by its actual durable position and original episode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryClaimRef {
    /// Canonical logical episode.
    pub episode: RecoveryEpisodeId,
    /// Exact authorization position.
    pub authorization_lsn: Lsn,
}

/// Supplemental historical context. Mandatory current answers are composed later.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRecap {
    /// At most eight KiB of UTF-8, possibly omitted by task packing.
    pub text: String,
    /// Exact recorded sources in increasing journal order.
    pub sources: Vec<Lsn>,
    /// Delivery proofs for exactly the historical answers listed among `sources`.
    pub delivered_answers: Vec<RecoveryAnswerDelivery>,
}

/// Proof that an answer was carried for this goal by a child that was durably recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_field_names,
    reason = "lsn suffixes distinguish journal source positions from answer, issue and spawn payloads"
)]
pub struct RecoveryAnswerDelivery {
    /// Exact historical answer.
    pub answer_lsn: Lsn,
    /// Issue whose ordered answer list carried it.
    pub issue_lsn: Lsn,
    /// Matching physical spawn, rather than current pending-answer presence.
    pub spawn_lsn: Lsn,
}

/// Recorded actor attribution. Deserialization and historical consistency confer no permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryAttribution {
    /// Local operator attribution captured by authenticated router admission.
    LocalOperator {
        /// Transport-resolved peer process.
        pid: u32,
        /// Transport-resolved user identity, when available.
        #[serde(deserialize_with = "required_option")]
        #[schemars(with = "crate::chat::ExplicitNullable<u32>")]
        uid: Option<u32>,
    },
    /// Browser session attribution captured by live session admission.
    BrowserSession {
        /// Digest of the admitted session, never its cookie or secret.
        session_digest: Hex32,
    },
}

/// An immutable context choice for new work, never a promise of restored history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryContext {
    /// Try only this still-confirmed recorded conversation.
    TryRecorded {
        /// Exact internal vendor-confirmed identifier, never a browser input.
        session: SessionId,
    },
    /// New vendor context with bounded supplemental recap intent.
    Fresh {
        /// Daemon-minted identifier, or vendor-assigned context.
        #[serde(deserialize_with = "required_option")]
        #[schemars(with = "crate::chat::ExplicitNullable<SessionId>")]
        session: Option<SessionId>,
        /// Historical recap intent, packed only after mandatory task sections.
        recap: RecoveryRecap,
    },
}

/// Compact evidence of the immutable issued task and its ordered carried answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryIssued {
    /// Original owner, goal and turn.
    pub work: RecoveryWork,
    /// Digest of exact task bytes, computed by the journal adapter.
    pub task_digest: Hex32,
    /// Exact ordered answer list carried by that issue.
    pub answers: Vec<Lsn>,
    /// Original explicit model selection.
    #[serde(deserialize_with = "strict_model")]
    #[schemars(with = "crate::chat::ExplicitNullable<RecoveryModelFields>")]
    pub model_selection: Option<ModelSelection>,
    /// Session announced for this new issue, without a provider-history claim.
    #[serde(deserialize_with = "required_option")]
    #[schemars(with = "crate::chat::ExplicitNullable<SessionId>")]
    pub session: Option<SessionId>,
}

/// An actual durable issue, without predicting its append position.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryIssue {
    /// Actual managed issue position.
    pub lsn: Lsn,
    /// Exact immutable payload witness.
    pub issued: RecoveryIssued,
}

/// One new planned turn or one already-issued immutable obligation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryPlan {
    /// Normal admission later composes task bytes and carried answers.
    New {
        /// Reserved identity, without another admission or budget charge yet.
        turn: TurnId,
        /// Exact context and supplemental recap intent.
        context: RecoveryContext,
    },
    /// Fresh-context repeat without a new issue, task, hop or reserve.
    RepeatIssued {
        /// Exact original issue witness.
        issue: RecoveryIssue,
        /// Fresh launch announcement, independent of the original issue's announcement.
        #[serde(deserialize_with = "required_option")]
        #[schemars(with = "crate::chat::ExplicitNullable<SessionId>")]
        session: Option<SessionId>,
    },
}

/// Immutable intent that exclusively claims one episode after fsync.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryAuthorized {
    /// Exact canonical episode, including its original boundary.
    pub episode: RecoveryEpisodeId,
    /// Original target and configuration.
    pub binding: RecoveryBinding,
    /// Original operator request identity.
    pub request_id: Hex32,
    /// Digest of canonical request fields.
    pub request_digest: Hex32,
    /// Recorded identity; a writer must separately authenticate the live command.
    pub attribution: RecoveryAttribution,
    /// Control identity the operator reviewed.
    #[serde(deserialize_with = "required_option")]
    #[schemars(with = "crate::chat::ExplicitNullable<Lsn>")]
    pub expected_control: Option<Lsn>,
    /// Control identity after this command's optional attributed Resume.
    #[serde(deserialize_with = "required_option")]
    #[schemars(with = "crate::chat::ExplicitNullable<Lsn>")]
    pub resulting_control: Option<Lsn>,
    /// Exact own Resume, absent when no control transition was needed.
    #[serde(deserialize_with = "required_option")]
    #[schemars(with = "crate::chat::ExplicitNullable<Lsn>")]
    pub resume_lsn: Option<Lsn>,
    /// New work intent or an immutable owed issue.
    pub plan: RecoveryPlan,
}

/// A durable relationship change that closes unused consent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryInvalidationReason {
    /// The original goal was replaced, cleared or reassigned.
    GoalChanged,
    /// The existing predicate rule qualified an observed completion of this goal.
    GoalCompleted,
    /// The original ticket association no longer names this target.
    TicketChanged,
    /// The original daemon-owned lease was removed.
    LeaseChanged,
    /// Another control transition changed the reviewed identity.
    ControlChanged,
    /// The agent was retired or attached.
    AgentUnavailable,
}

/// Journaled evidence and a durable authority observation have different position semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryInvalidationEvidence {
    /// An actual earlier record supplies the named invalidating relationship.
    RecordedChange {
        /// Fixed domain reason, validated against the source kind and owner.
        reason: RecoveryInvalidationReason,
        /// Actual preceding evidence, never the invalidation's own position.
        lsn: Lsn,
    },
    /// This invalidation durably records a daemon-owned effective-authority recheck.
    BrowserAuthorityUnavailable {
        /// Exact originating browser session; no guessed loss time or cause.
        session_digest: Hex32,
    },
}

/// Permanent closure of one unused claim, without abandoning any owed work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryInvalidated {
    /// Exact original authorization and episode.
    pub claim: RecoveryClaimRef,
    /// Actual recorded change or observation made durable by this record.
    pub evidence: RecoveryInvalidationEvidence,
}

/// Check position-independent recovery payload bounds before staging or historical folding.
/// This does not establish historical relationships or authenticate a caller.
pub fn validate_record_shape(
    record: &crate::Record,
) -> Result<(), crate::recovery_state::RecoveryStateError> {
    if let crate::Record::RecoveryAuthorized(authorization) = record
        && let RecoveryPlan::New {
            context: RecoveryContext::Fresh { recap, .. },
            ..
        } = &authorization.plan
        && recap.text.len() > MAX_RECOVERY_RECAP_BYTES
    {
        return Err(crate::recovery_state::RecoveryStateError::Context);
    }
    Ok(())
}

fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

fn strict_ticket<'de, D>(deserializer: D) -> Result<Option<TicketBinding>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<RecoveryTicketFields>::deserialize(deserializer).map(|value| {
        value.map(|fields| TicketBinding {
            card: fields.card,
            round: fields.round,
            goal: fields.goal,
            claimant: fields.claimant,
        })
    })
}

fn strict_model<'de, D>(deserializer: D) -> Result<Option<ModelSelection>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<RecoveryModelFields>::deserialize(deserializer).map(|value| {
        value.map(|fields| ModelSelection {
            selector: fields.selector,
        })
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RecoveryTicketFields {
    card: CardId,
    round: u32,
    goal: GoalId,
    claimant: AgentId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RecoveryModelFields {
    #[serde(default)]
    selector: Option<String>,
}
