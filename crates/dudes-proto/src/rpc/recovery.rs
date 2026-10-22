//! Bounded recovery intent and historical receipt views.

use std::fmt;
use std::marker::PhantomData;

use dudes_types::{AgentId, CardId, Lsn, TurnId};
use schemars::JsonSchema;
use serde::de::{MapAccess, Visitor, value::MapAccessDeserializer};
use serde::{Deserialize, Deserializer, Serialize};

use crate::chat::required_nullable;
use crate::record::Hex32;
use crate::recovery::{RecoveryClaimRef, RecoveryEpisodeId, RecoveryReason, RecoveryWork};

/// Maximum original recovery JSON request bytes, excluding the LF or CRLF delimiter.
///
/// This bounds the complete JSON envelope, including whitespace and escape bytes before decoding.
pub const MAX_RECOVERY_REQUEST_BYTES: usize = 4096;

/// Target of one inert recovery inspection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryInspectParams {
    /// Agent whose current opportunity is inspected.
    pub agent: AgentId,
}

/// Ticket round shown with the interrupted work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewedTicketRound {
    /// Exact ticket.
    pub card: CardId,
    /// Exact reviewed round.
    #[schemars(range(min = u32::MIN, max = u32::MAX))]
    pub round: u32,
}

/// Conditions the operator reviewed before choosing recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExpectedRecovery {
    /// Original agent, goal and interrupted turn.
    pub work: RecoveryWork,
    /// Exact original issue.
    pub issue_lsn: Lsn,
    /// Observed eligibility source, never a predicted barrier position.
    pub source_lsn: Lsn,
    /// Current canonical episode, explicitly null before one exists.
    #[schemars(with = "crate::chat::ExplicitNullable<RecoveryEpisodeId>")]
    pub episode: Option<RecoveryEpisodeId>,
    /// Exact claim a new barrier would supersede, explicitly null when unclaimed.
    #[schemars(with = "crate::chat::ExplicitNullable<RecoveryClaimRef>")]
    pub expected_predecessor: Option<RecoveryClaimRef>,
    /// Reviewed control identity, explicitly null without a recorded control.
    #[schemars(with = "crate::chat::ExplicitNullable<Lsn>")]
    pub control_lsn: Option<Lsn>,
    /// Reviewed ticket association, explicitly null for non-ticket work.
    #[schemars(with = "crate::chat::ExplicitNullable<ReviewedTicketRound>")]
    pub ticket: Option<ReviewedTicketRound>,
}

/// An explicit context choice, without caller-supplied context contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryChoice {
    /// Try the recorded confirmed conversation.
    TryRecorded,
    /// Start fresh with daemon-generated supplemental recap.
    FreshWithRecap,
    /// Repeat the immutable owed issue in fresh context.
    RepeatIssuedFresh,
}

/// One deliberate submission. Parsing does not establish eligibility or caller authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoverySubmitParams {
    /// Identity retained before sending, including when the reply is lost.
    pub request_id: Hex32,
    /// Exact conditions returned by inspection.
    pub expected: ExpectedRecovery,
    /// Operator-selected context mode.
    pub choice: RecoveryChoice,
}

/// An inert lookup using only the identity retained before submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryReceiptParams {
    /// Original request identity.
    pub request_id: Hex32,
}

/// Complete finite sets of choices offered for one opportunity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAllowedChoices {
    /// Only fresh context with supplemental recap is offered.
    FreshOnly,
    /// Recorded context and fresh context with supplemental recap are offered.
    RecordedOrFresh,
    /// Only a fresh repeat of the owed issue is offered.
    RepeatIssuedFresh,
}

/// Current admission waits. Authorization does not clear these waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryWaitSummary {
    /// A question still needs an answer.
    pub open_question: RecoveryWaitStatus,
    /// An admission budget is exhausted.
    pub budget: RecoveryWaitStatus,
    /// The goal has no remaining hops.
    pub hops: RecoveryWaitStatus,
    /// Ticket instructions still need review.
    pub instruction_review: RecoveryWaitStatus,
    /// A ticket instruction comparison is outstanding.
    pub instruction_comparison: RecoveryWaitStatus,
}

/// Whether one admission gate is currently waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryWaitStatus {
    /// This gate is not waiting.
    Ready,
    /// This gate still prevents admission.
    Waiting,
}

/// Fixed explanation when inspection cannot offer recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryInspectUnavailable {
    /// The agent is absent, retired or not managed.
    AgentUnavailable,
    /// No unfinished current goal can be recovered.
    GoalUnavailable,
    /// The agent has a standing halt.
    Halted,
    /// A physical attempt or command check remains unsettled.
    TurnOpen,
    /// The current boundary does not permit recovery.
    NotRecoverable,
    /// Existing consent exclusively owns this opportunity.
    Claimed,
    /// Journal or live facts cannot be checked coherently.
    Unavailable,
}

/// One current opportunity. The inspected cut alone is not a staleness condition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RecoveryInspectResult {
    /// One opportunity with exact reviewed conditions and bounded waits.
    Available {
        /// Informational checked journal cut.
        through: Lsn,
        /// Conditions that submission must preserve.
        expected: Box<ExpectedRecovery>,
        /// Recorded boundary restriction.
        reason: RecoveryReason,
        /// Complete offered choice set.
        choices: RecoveryAllowedChoices,
        /// Prior external effects may repeat.
        prior_effects_unknown: bool,
        /// Admission waits still in force.
        waiting: RecoveryWaitSummary,
    },
    /// No coherent eligible opportunity is offered.
    Unavailable {
        /// Fixed explanation without raw errors.
        reason: RecoveryInspectUnavailable,
    },
}

/// Fixed historical closure reason without browser-session identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryReceiptInvalidation {
    /// The original goal changed.
    GoalChanged,
    /// An observed predicate completed the goal.
    GoalCompleted,
    /// The ticket association changed.
    TicketChanged,
    /// The recorded checkout lease changed.
    LeaseChanged,
    /// The reviewed control changed.
    ControlChanged,
    /// The agent became unavailable.
    AgentUnavailable,
    /// Effective browser authority could not be revalidated.
    AuthorityUnavailable,
}

/// Bounded committed lifecycle without task, answer or context contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RecoveryReceiptState {
    /// The reserved new turn has not been issued.
    Pending,
    /// An exact issue is durable without a consuming physical receipt.
    IssueBound {
        /// Actual issued turn.
        turn: TurnId,
        /// Actual issue position.
        issue_lsn: Lsn,
    },
    /// A matching physical attempt was durably observed.
    Consumed {
        /// Actual issued turn.
        turn: TurnId,
        /// Actual issue position.
        issue_lsn: Lsn,
        /// Recorded physical receipt position.
        evidence_lsn: Lsn,
    },
    /// Unused consent closed on recorded evidence.
    Invalidated {
        /// Actual invalidation position.
        invalidation_lsn: Lsn,
        /// Fixed closure reason.
        reason: RecoveryReceiptInvalidation,
    },
    /// A later missing-Spawn episode superseded this consent.
    Superseded {
        /// Unchanged owed turn.
        turn: TurnId,
        /// Unchanged owed issue position.
        issue_lsn: Lsn,
        /// Exact successor episode.
        successor: RecoveryEpisodeId,
        /// Actual superseding barrier position.
        barrier_lsn: Lsn,
    },
}

/// Recorded physical-attempt positions, without a provider-history guarantee.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryAttemptView {
    /// Latest recorded spawn, if any.
    pub spawn: Option<Lsn>,
    /// Accepted confirmation for the current attempt, if any.
    pub confirmation: Option<Lsn>,
    /// Latest recorded end, if any.
    pub end: Option<Lsn>,
}

/// Durable authorization and its state at a checked cut. This does not promise a launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryReceiptView {
    /// Original request identity.
    pub request_id: Hex32,
    /// Original authorization identity.
    pub claim: RecoveryClaimRef,
    /// Original selected context mode.
    pub choice: RecoveryChoice,
    /// Current durable claim lifecycle.
    pub status: RecoveryReceiptState,
    /// Recorded attempt facts.
    pub attempt: RecoveryAttemptView,
    /// Prior external effects may repeat.
    pub prior_effects_unknown: bool,
}

/// Fixed reason a receipt read cannot establish presence or absence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryReadUnavailable {
    /// A journal write has uncertain durability.
    JournalUncertain,
    /// The persistent generation lacks a valid certificate.
    Uncertified,
    /// The journal advanced beyond the certified cut.
    StaleCut,
    /// Metadata or historical relationships are inconsistent.
    InvalidHistory,
    /// Persistent storage could not be read.
    StorageUnavailable,
}

/// Result of an inert historical lookup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RecoveryReceiptResult {
    /// Original durable authorization at this checked cut.
    Found {
        /// Bounded historical receipt.
        receipt: Box<RecoveryReceiptView>,
    },
    /// No authorization exists at this certain complete cut.
    Absent {
        /// Exact cut checked for the request.
        checked_through: Lsn,
    },
    /// Neither presence nor absence can be certified.
    Unavailable {
        /// Fixed explanation without raw errors.
        reason: RecoveryReadUnavailable,
    },
}

/// Known acknowledged prefix of this submission, including a later refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryKnownPrefix {
    /// Actual acknowledged barrier, if this command wrote one.
    pub required: Option<Lsn>,
    /// Actual acknowledged own Resume, if this command wrote one.
    pub resume: Option<Lsn>,
}

/// Fixed semantic refusal of one submission attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoverySubmitRefusal {
    /// This identity already names different intent.
    RequestConflict,
    /// The original work or issue no longer matches.
    StaleWork,
    /// The inspected eligibility source no longer matches.
    StaleSource,
    /// The canonical episode no longer matches.
    StaleEpisode,
    /// The claim to supersede no longer matches.
    StalePredecessor,
    /// The reviewed control no longer matches.
    StaleControl,
    /// The reviewed ticket round no longer matches.
    StaleTicket,
    /// Existing consent exclusively owns this opportunity.
    Claimed,
    /// The selected context mode is not offered.
    ChoiceUnavailable,
    /// The current work cannot be recovered.
    NotRecoverable,
    /// The admitted authority could not be revalidated.
    AuthorityUnavailable,
    /// Journal or live facts cannot be checked coherently.
    Unavailable,
}

/// Outcome of one deliberate submission, without automatic retry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RecoverySubmitResult {
    /// Durable authorization was certified.
    Accepted {
        /// Bounded original receipt.
        receipt: Box<RecoveryReceiptView>,
    },
    /// This attempt refused while retaining its acknowledged prefix.
    Refused {
        /// Fixed refusal reason.
        reason: RecoverySubmitRefusal,
        /// Frames already acknowledged for this invocation.
        prefix: RecoveryKnownPrefix,
    },
    /// Authorization may be durable but cannot currently be certified.
    Unknown {
        /// Retained identity for a separate receipt read.
        request_id: Hex32,
    },
}

fn object<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Object<T>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for Object<T> {
        type Value = T;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("one JSON object")
        }
        fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<T, M::Error> {
            T::deserialize(MapAccessDeserializer::new(map))
        }
    }
    deserializer.deserialize_map(Object(PhantomData))
}

struct InputEpisode(RecoveryEpisodeId);

impl<'de> Deserialize<'de> for InputEpisode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            #[serde(deserialize_with = "object")]
            work: RecoveryWork,
            origin: Lsn,
        }
        let fields: Fields = object(deserializer)?;
        Ok(Self(RecoveryEpisodeId {
            work: fields.work,
            origin: fields.origin,
        }))
    }
}

struct InputClaim(RecoveryClaimRef);

impl<'de> Deserialize<'de> for InputClaim {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            episode: InputEpisode,
            authorization_lsn: Lsn,
        }
        let fields: Fields = object(deserializer)?;
        Ok(Self(RecoveryClaimRef {
            episode: fields.episode.0,
            authorization_lsn: fields.authorization_lsn,
        }))
    }
}

impl<'de> Deserialize<'de> for RecoveryInspectParams {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            agent: AgentId,
        }
        let fields: Fields = object(deserializer)?;
        Ok(Self {
            agent: fields.agent,
        })
    }
}

impl<'de> Deserialize<'de> for ReviewedTicketRound {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            card: CardId,
            round: u32,
        }
        let fields: Fields = object(deserializer)?;
        Ok(Self {
            card: fields.card,
            round: fields.round,
        })
    }
}

impl<'de> Deserialize<'de> for ExpectedRecovery {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            #[serde(deserialize_with = "object")]
            work: RecoveryWork,
            issue_lsn: Lsn,
            source_lsn: Lsn,
            #[serde(deserialize_with = "required_nullable")]
            episode: Option<InputEpisode>,
            #[serde(deserialize_with = "required_nullable")]
            expected_predecessor: Option<InputClaim>,
            #[serde(deserialize_with = "required_nullable")]
            control_lsn: Option<Lsn>,
            #[serde(deserialize_with = "required_nullable")]
            ticket: Option<ReviewedTicketRound>,
        }
        let fields: Fields = object(deserializer)?;
        Ok(Self {
            work: fields.work,
            issue_lsn: fields.issue_lsn,
            source_lsn: fields.source_lsn,
            episode: fields.episode.map(|episode| episode.0),
            expected_predecessor: fields.expected_predecessor.map(|claim| claim.0),
            control_lsn: fields.control_lsn,
            ticket: fields.ticket,
        })
    }
}

impl<'de> Deserialize<'de> for RecoverySubmitParams {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            request_id: Hex32,
            expected: ExpectedRecovery,
            choice: RecoveryChoice,
        }
        let fields: Fields = object(deserializer)?;
        Ok(Self {
            request_id: fields.request_id,
            expected: fields.expected,
            choice: fields.choice,
        })
    }
}

impl<'de> Deserialize<'de> for RecoveryReceiptParams {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            request_id: Hex32,
        }
        let fields: Fields = object(deserializer)?;
        Ok(Self {
            request_id: fields.request_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const NULL_SUBMIT: &str = r#"{"request_id":"0000000000000000000000000000000000000000000000000000000000000000","expected":{"work":{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","goal":"01ARZ3NDEKTSV4RRFFQ69G5FAW","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX"},"issue_lsn":20,"source_lsn":80,"episode":null,"expected_predecessor":null,"control_lsn":null,"ticket":null},"choice":"fresh_with_recap"}"#;
    const BOUND_SUBMIT: &str = r#"{"request_id":"0000000000000000000000000000000000000000000000000000000000000000","expected":{"work":{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","goal":"01ARZ3NDEKTSV4RRFFQ69G5FAW","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX"},"issue_lsn":20,"source_lsn":80,"episode":{"work":{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","goal":"01ARZ3NDEKTSV4RRFFQ69G5FAW","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX"},"origin":90},"expected_predecessor":{"episode":{"work":{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","goal":"01ARZ3NDEKTSV4RRFFQ69G5FAW","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX"},"origin":90},"authorization_lsn":300},"control_lsn":90,"ticket":{"card":"01ARZ3NDEKTSV4RRFFQ69G5FAY","round":2}},"choice":"repeat_issued_fresh"}"#;

    #[test]
    fn submission_preserves_literal_conditions_and_explicit_nulls() {
        let parsed: RecoverySubmitParams = serde_json::from_str(NULL_SUBMIT).unwrap();
        assert_eq!(parsed.request_id, Hex32::new([0; 32]));
        assert_eq!(parsed.choice, RecoveryChoice::FreshWithRecap);
        assert_eq!(parsed.expected.issue_lsn, Lsn(20));
        assert_eq!(parsed.expected.source_lsn, Lsn(80));
        assert_eq!(parsed.expected.episode, None);
        assert_eq!(parsed.expected.expected_predecessor, None);
        assert_eq!(parsed.expected.control_lsn, None);
        assert_eq!(parsed.expected.ticket, None);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), NULL_SUBMIT);

        let parsed: RecoverySubmitParams = serde_json::from_str(BOUND_SUBMIT).unwrap();
        assert_eq!(parsed.choice, RecoveryChoice::RepeatIssuedFresh);
        assert_eq!(parsed.expected.episode.unwrap().origin, Lsn(90));
        assert_eq!(
            parsed
                .expected
                .expected_predecessor
                .unwrap()
                .authorization_lsn,
            Lsn(300)
        );
        assert_eq!(parsed.expected.control_lsn, Some(Lsn(90)));
        assert_eq!(parsed.expected.ticket.unwrap().round, 2);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), BOUND_SUBMIT);
    }

    #[test]
    fn authority_options_require_presence_in_both_decoder_and_schema() {
        let schema = schemars::schema_for!(RecoverySubmitParams).to_value();
        let validator = jsonschema::validator_for(&schema).unwrap();
        for original in [NULL_SUBMIT, BOUND_SUBMIT] {
            let good: Value = serde_json::from_str(original).unwrap();
            assert!(validator.is_valid(&good));
            for key in ["episode", "expected_predecessor", "control_lsn", "ticket"] {
                let mut omitted = good.clone();
                omitted["expected"].as_object_mut().unwrap().remove(key);
                assert!(!validator.is_valid(&omitted), "{key}");
                assert!(
                    serde_json::from_slice::<RecoverySubmitParams>(
                        &serde_json::to_vec(&omitted).unwrap()
                    )
                    .is_err(),
                    "{key}"
                );
            }
        }
        let mut required: Vec<_> = schema["$defs"]["ExpectedRecovery"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|field| field.as_str().unwrap())
            .collect();
        required.sort_unstable();
        assert_eq!(
            required,
            [
                "control_lsn",
                "episode",
                "expected_predecessor",
                "issue_lsn",
                "source_lsn",
                "ticket",
                "work"
            ]
        );
        for (key, escaped) in [
            ("episode", r"episo\u0064e"),
            ("expected_predecessor", r"expected_predecesso\u0072"),
            ("control_lsn", r"control_\u006csn"),
            ("ticket", r"ticke\u0074"),
        ] {
            for inserted in [key, escaped] {
                let duplicate = NULL_SUBMIT.replace(
                    &format!(r#""{key}":null"#),
                    &format!(r#""{key}":null,"{inserted}":null"#),
                );
                let error = serde_json::from_str::<RecoverySubmitParams>(&duplicate).unwrap_err();
                assert!(error.to_string().contains("duplicate field"), "{error}");
            }
        }
    }

    #[test]
    fn original_json_rejects_unknown_duplicate_and_escaped_keys_at_every_authority_object() {
        assert!(serde_json::from_str::<RecoverySubmitParams>(BOUND_SUBMIT).is_ok());
        for (prefix, key, escaped, value) in [
            (
                "{",
                "request_id",
                r"request_\u0069d",
                r#""0000000000000000000000000000000000000000000000000000000000000000""#,
            ),
            (r#""expected":{"#, "issue_lsn", r"issue_\u006csn", "20"),
            (
                r#""work":{"#,
                "agent",
                r"a\u0067ent",
                r#""01ARZ3NDEKTSV4RRFFQ69G5FAV""#,
            ),
            (r#""episode":{"#, "origin", r"ori\u0067in", "90"),
            (
                r#""episode":{"work":{"#,
                "agent",
                r"a\u0067ent",
                r#""01ARZ3NDEKTSV4RRFFQ69G5FAV""#,
            ),
            (
                r#""expected_predecessor":{"#,
                "authorization_lsn",
                r"authorization_\u006csn",
                "300",
            ),
            (
                r#""expected_predecessor":{"episode":{"#,
                "origin",
                r"ori\u0067in",
                "90",
            ),
            (
                r#""expected_predecessor":{"episode":{"work":{"#,
                "agent",
                r"a\u0067ent",
                r#""01ARZ3NDEKTSV4RRFFQ69G5FAV""#,
            ),
            (r#""ticket":{"#, "round", r"ro\u0075nd", "2"),
        ] {
            for member in [
                r#""extra":0"#.to_owned(),
                format!(r#""{key}":{value}"#),
                format!(r#""{escaped}":{value}"#),
            ] {
                let changed = BOUND_SUBMIT.replacen(prefix, &format!("{prefix}{member},"), 1);
                assert_ne!(changed, BOUND_SUBMIT);
                assert!(
                    serde_json::from_str::<RecoverySubmitParams>(&changed).is_err(),
                    "{prefix} {member}"
                );
            }
        }
    }

    #[test]
    fn submission_decoder_and_schema_reject_array_objects_and_unbounded_authority() {
        let schema = schemars::schema_for!(RecoverySubmitParams).to_value();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let good: Value = serde_json::from_str(BOUND_SUBMIT).unwrap();
        assert!(validator.is_valid(&good));
        for round in [0, 4_294_967_295_u32] {
            let mut endpoint = good.clone();
            endpoint["expected"]["ticket"]["round"] = json!(round);
            assert!(validator.is_valid(&endpoint), "{round}");
            let parsed: RecoverySubmitParams =
                serde_json::from_slice(&serde_json::to_vec(&endpoint).unwrap()).unwrap();
            assert_eq!(parsed.expected.ticket.unwrap().round, round);
        }
        for (pointer, fields) in [
            ("", &["request_id", "expected", "choice"][..]),
            (
                "/expected",
                &[
                    "work",
                    "issue_lsn",
                    "source_lsn",
                    "episode",
                    "expected_predecessor",
                    "control_lsn",
                    "ticket",
                ][..],
            ),
            ("/expected/work", &["agent", "goal", "turn"][..]),
            ("/expected/episode", &["work", "origin"][..]),
            ("/expected/episode/work", &["agent", "goal", "turn"][..]),
            (
                "/expected/expected_predecessor",
                &["episode", "authorization_lsn"][..],
            ),
            (
                "/expected/expected_predecessor/episode",
                &["work", "origin"][..],
            ),
            (
                "/expected/expected_predecessor/episode/work",
                &["agent", "goal", "turn"][..],
            ),
            ("/expected/ticket", &["card", "round"][..]),
        ] {
            let mut bad = good.clone();
            let object = bad.pointer_mut(pointer).unwrap();
            *object = Value::Array(fields.iter().map(|field| object[*field].clone()).collect());
            assert!(!validator.is_valid(&bad), "{pointer}");
            assert!(
                serde_json::from_slice::<RecoverySubmitParams>(&serde_json::to_vec(&bad).unwrap())
                    .is_err(),
                "{pointer}"
            );
        }
        for (pointer, value) in [
            ("/choice", json!("fresh")),
            (
                "/choice",
                json!({"fresh_with_recap":{"text":"caller recap"}}),
            ),
            ("/request_id", json!("A".repeat(64))),
            ("/expected/work/agent", json!("01arz3ndektsv4rrffq69g5fav")),
            ("/expected/ticket/round", json!(-1)),
            ("/expected/ticket/round", json!(u64::from(u32::MAX) + 1)),
        ] {
            let mut bad = good.clone();
            *bad.pointer_mut(pointer).unwrap() = value;
            assert!(!validator.is_valid(&bad), "{pointer}");
            assert!(
                serde_json::from_slice::<RecoverySubmitParams>(&serde_json::to_vec(&bad).unwrap())
                    .is_err(),
                "{pointer}"
            );
        }
        for key in [
            "task", "path", "session", "model", "actor", "recap", "digest", "plan",
        ] {
            let original = NULL_SUBMIT.replacen('{', &format!(r#"{{"{key}":null,"#), 1);
            assert!(
                serde_json::from_str::<RecoverySubmitParams>(&original).is_err(),
                "{key}"
            );
        }
    }

    #[test]
    fn inspection_and_receipt_queries_are_strict_single_identity_objects() {
        let inspection = r#"{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}"#;
        let receipt =
            r#"{"request_id":"0000000000000000000000000000000000000000000000000000000000000000"}"#;
        let parsed: RecoveryInspectParams = serde_json::from_str(inspection).unwrap();
        assert_eq!(serde_json::to_string(&parsed).unwrap(), inspection);
        let parsed: RecoveryReceiptParams = serde_json::from_str(receipt).unwrap();
        assert_eq!(parsed.request_id, Hex32::new([0; 32]));
        assert_eq!(serde_json::to_string(&parsed).unwrap(), receipt);
        let inspection_schema = schemars::schema_for!(RecoveryInspectParams).to_value();
        let receipt_schema = schemars::schema_for!(RecoveryReceiptParams).to_value();
        assert!(
            jsonschema::validator_for(&inspection_schema)
                .unwrap()
                .is_valid(&serde_json::from_str::<Value>(inspection).unwrap())
        );
        assert!(
            jsonschema::validator_for(&receipt_schema)
                .unwrap()
                .is_valid(&serde_json::from_str::<Value>(receipt).unwrap())
        );
        for bad in [
            r"{}".into(),
            r"[]".into(),
            inspection.replacen('{', r#"{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","#, 1),
            inspection.replacen('{', r#"{"a\u0067ent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","#, 1),
            inspection.replacen('{', r#"{"extra":0,"#, 1),
        ] {
            assert!(
                serde_json::from_str::<RecoveryInspectParams>(&bad).is_err(),
                "{bad}"
            );
        }
        for bad in [r"{}".into(), r"[]".into(), receipt.replacen('{', r#"{"request_id":"0000000000000000000000000000000000000000000000000000000000000000","#, 1), receipt.replacen('{', r#"{"request_\u0069d":"0000000000000000000000000000000000000000000000000000000000000000","#, 1), receipt.replacen('{', r#"{"digest":null,"#, 1)] {
            assert!(serde_json::from_str::<RecoveryReceiptParams>(&bad).is_err(), "{bad}");
        }
    }

    fn assert_extensible_result<T>(schema: &Value, original: &Value)
    where
        T: serde::de::DeserializeOwned + Serialize,
    {
        let validator = jsonschema::validator_for(schema).unwrap();
        assert!(validator.is_valid(original));
        let mut extended = original.clone();
        extended["future"] = json!(true);
        if let Some(receipt) = extended.get_mut("receipt") {
            receipt["future"] = json!(true);
            receipt["attempt"]["future"] = json!(true);
            receipt["status"]["future"] = json!(true);
        }
        assert!(validator.is_valid(&extended));
        let parsed: T = serde_json::from_value(extended).unwrap();
        assert_eq!(&serde_json::to_value(parsed).unwrap(), original);
    }

    #[test]
    fn bounded_results_cover_lifecycle_and_keep_result_growth() {
        let submit: RecoverySubmitParams = serde_json::from_str(BOUND_SUBMIT).unwrap();
        let claim = submit.expected.expected_predecessor.unwrap();
        let states = [
            json!({"state":"pending"}),
            json!({"state":"issue_bound","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX","issue_lsn":20}),
            json!({"state":"consumed","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX","issue_lsn":20,"evidence_lsn":400}),
            json!({"state":"invalidated","invalidation_lsn":400,"reason":"authority_unavailable"}),
            json!({"state":"superseded","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX","issue_lsn":20,"successor":claim.episode,"barrier_lsn":400}),
        ];
        let receipt_schema = schemars::schema_for!(RecoveryReceiptResult).to_value();
        let submit_schema = schemars::schema_for!(RecoverySubmitResult).to_value();
        for status in states {
            let view = json!({"request_id":"0".repeat(64),"claim":claim,"choice":"repeat_issued_fresh","status":status,"attempt":{"spawn":null,"confirmation":null,"end":null},"prior_effects_unknown":true});
            let found = json!({"state":"found","receipt":view});
            assert_extensible_result::<RecoveryReceiptResult>(&receipt_schema, &found);
            let accepted = json!({"state":"accepted","receipt":view});
            assert_extensible_result::<RecoverySubmitResult>(&submit_schema, &accepted);
        }
        for result in [
            json!({"state":"absent","checked_through":500}),
            json!({"state":"unavailable","reason":"invalid_history"}),
        ] {
            assert_extensible_result::<RecoveryReceiptResult>(&receipt_schema, &result);
        }
        for result in [
            json!({"state":"refused","reason":"stale_predecessor","prefix":{"required":200,"resume":250}}),
            json!({"state":"unknown","request_id":"0".repeat(64)}),
        ] {
            assert_extensible_result::<RecoverySubmitResult>(&submit_schema, &result);
        }
        let inspection_schema = schemars::schema_for!(RecoveryInspectResult).to_value();
        for result in [
            json!({"state":"available","through":500,"expected":submit.expected,"reason":"missing_spawn","choices":"repeat_issued_fresh","prior_effects_unknown":true,"waiting":{"open_question":"waiting","budget":"ready","hops":"ready","instruction_review":"ready","instruction_comparison":"waiting"}}),
            json!({"state":"unavailable","reason":"claimed"}),
        ] {
            assert_extensible_result::<RecoveryInspectResult>(&inspection_schema, &result);
        }
        for schema in [receipt_schema, submit_schema, inspection_schema] {
            let text = serde_json::to_string(&schema).unwrap();
            for forbidden in [
                "RecoveryAuthorized",
                "RecoveryRecap",
                "RecoveryAttribution",
                "task_digest",
                "session_digest",
                "model_selection",
                "delivered_answers",
            ] {
                assert!(!text.contains(forbidden), "{forbidden}");
            }
        }
    }
}
