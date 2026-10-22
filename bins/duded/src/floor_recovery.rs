//! Lossless JSON transport for the Floor's recovery requests and finite replies.

use std::fmt;
use std::marker::PhantomData;

use anyhow::{Result, bail};
use dudes_proto::record::Hex32;
use dudes_proto::recovery::{RecoveryClaimRef, RecoveryEpisodeId, RecoveryReason, RecoveryWork};
use dudes_proto::rpc::recovery::{
    ExpectedRecovery, RecoveryAllowedChoices, RecoveryChoice, RecoveryInspectParams,
    RecoveryInspectResult, RecoveryInspectUnavailable, RecoveryReadUnavailable,
    RecoveryReceiptInvalidation, RecoveryReceiptParams, RecoveryReceiptResult,
    RecoveryReceiptState, RecoveryReceiptView, RecoverySubmitParams, RecoverySubmitRefusal,
    RecoverySubmitResult, RecoveryWaitSummary, ReviewedTicketRound,
};
use dudes_types::{Lsn, TurnId};
use serde::de::{DeserializeOwned, MapAccess, Visitor, value::MapAccessDeserializer};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum original HTTP request body bytes, including all whitespace and line endings.
pub const MAX_RECOVERY_BODY_BYTES: usize = 4096;
/// Maximum encoded recovery response bytes.
pub const MAX_RECOVERY_RESPONSE_BYTES: usize = 4096;

/// Decode one strict inspection object from the original bounded HTTP body.
///
/// # Errors
/// Returns an error for oversized, malformed, unknown, duplicate, or positional input.
pub fn decode_inspect(bytes: &[u8]) -> Result<RecoveryInspectParams> {
    decode(bytes)
}

/// Decode one strict submission, converting canonical decimal-string positions losslessly.
///
/// # Errors
/// Returns an error for invalid JSON, noncanonical positions, or incomplete authority fields.
pub fn decode_submit(bytes: &[u8]) -> Result<RecoverySubmitParams> {
    let Object(raw): Object<FloorSubmit> = decode(bytes)?;
    Ok(RecoverySubmitParams {
        request_id: raw.request_id,
        expected: raw.expected.0.into(),
        choice: raw.choice,
    })
}

/// Decode one strict receipt lookup containing only the retained request identity.
///
/// # Errors
/// Returns an error for oversized, malformed, unknown, duplicate, or positional input.
pub fn decode_receipt(bytes: &[u8]) -> Result<RecoveryReceiptParams> {
    decode(bytes)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    if bytes.len() > MAX_RECOVERY_BODY_BYTES {
        bail!("Floor recovery request is too large");
    }
    Ok(serde_json::from_slice(bytes)?)
}

#[derive(Serialize)]
#[serde(transparent)]
struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
            type Value = Object<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("one JSON object")
            }

            fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(Object)
            }
        }
        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}

#[derive(Clone, Copy)]
struct Position(Lsn);

impl Serialize for Position {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0.0)
    }
}

impl<'de> Deserialize<'de> for Position {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let value = text.parse::<u64>().map_err(serde::de::Error::custom)?;
        if text != value.to_string() {
            return Err(serde::de::Error::custom("noncanonical recovery position"));
        }
        Ok(Self(Lsn(value)))
    }
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FloorSubmit {
    request_id: Hex32,
    expected: Object<FloorExpected>,
    choice: RecoveryChoice,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FloorExpected {
    work: Object<RecoveryWork>,
    issue_lsn: Position,
    source_lsn: Position,
    #[serde(deserialize_with = "required_nullable")]
    episode: Option<Object<FloorEpisode>>,
    #[serde(deserialize_with = "required_nullable")]
    expected_predecessor: Option<Object<FloorClaim>>,
    #[serde(deserialize_with = "required_nullable")]
    control_lsn: Option<Position>,
    #[serde(deserialize_with = "required_nullable")]
    ticket: Option<Object<ReviewedTicketRound>>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FloorEpisode {
    work: Object<RecoveryWork>,
    origin: Position,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FloorClaim {
    episode: Object<FloorEpisode>,
    authorization_lsn: Position,
}

impl From<FloorExpected> for ExpectedRecovery {
    fn from(value: FloorExpected) -> Self {
        Self {
            work: value.work.0,
            issue_lsn: value.issue_lsn.0,
            source_lsn: value.source_lsn.0,
            episode: value.episode.map(|value| value.0.into()),
            expected_predecessor: value.expected_predecessor.map(|value| value.0.into()),
            control_lsn: value.control_lsn.map(|value| value.0),
            ticket: value.ticket.map(|value| value.0),
        }
    }
}

impl From<ExpectedRecovery> for FloorExpected {
    fn from(value: ExpectedRecovery) -> Self {
        Self {
            work: Object(value.work),
            issue_lsn: Position(value.issue_lsn),
            source_lsn: Position(value.source_lsn),
            episode: value.episode.map(|value| Object(value.into())),
            expected_predecessor: value.expected_predecessor.map(|value| Object(value.into())),
            control_lsn: value.control_lsn.map(Position),
            ticket: value.ticket.map(Object),
        }
    }
}

impl From<FloorEpisode> for RecoveryEpisodeId {
    fn from(value: FloorEpisode) -> Self {
        Self {
            work: value.work.0,
            origin: value.origin.0,
        }
    }
}

impl From<RecoveryEpisodeId> for FloorEpisode {
    fn from(value: RecoveryEpisodeId) -> Self {
        Self {
            work: Object(value.work),
            origin: Position(value.origin),
        }
    }
}

impl From<FloorClaim> for RecoveryClaimRef {
    fn from(value: FloorClaim) -> Self {
        Self {
            episode: value.episode.0.into(),
            authorization_lsn: value.authorization_lsn.0,
        }
    }
}

impl From<RecoveryClaimRef> for FloorClaim {
    fn from(value: RecoveryClaimRef) -> Self {
        Self {
            episode: Object(value.episode.into()),
            authorization_lsn: Position(value.authorization_lsn),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum FloorInspection {
    Available {
        through: Position,
        expected: Box<FloorExpected>,
        reason: RecoveryReason,
        choices: RecoveryAllowedChoices,
        prior_effects_unknown: bool,
        waiting: RecoveryWaitSummary,
    },
    Unavailable {
        reason: RecoveryInspectUnavailable,
    },
}

/// Encode the finite inspection view with decimal strings for all journal positions.
///
/// # Errors
/// Returns an error if serialization fails or the response exceeds its byte bound.
pub fn encode_inspection(result: &RecoveryInspectResult) -> Result<Vec<u8>> {
    let view = match result {
        RecoveryInspectResult::Available {
            through,
            expected,
            reason,
            choices,
            prior_effects_unknown,
            waiting,
        } => FloorInspection::Available {
            through: Position(*through),
            expected: Box::new((**expected).into()),
            reason: *reason,
            choices: *choices,
            prior_effects_unknown: *prior_effects_unknown,
            waiting: *waiting,
        },
        RecoveryInspectResult::Unavailable { reason } => {
            FloorInspection::Unavailable { reason: *reason }
        }
    };
    encode(&view)
}

#[derive(Serialize)]
struct FloorReceipt {
    request_id: Hex32,
    claim: FloorClaim,
    choice: RecoveryChoice,
    status: FloorReceiptState,
    attempt: FloorAttempt,
    prior_effects_unknown: bool,
}

#[derive(Serialize)]
struct FloorAttempt {
    spawn: Option<Position>,
    confirmation: Option<Position>,
    end: Option<Position>,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum FloorReceiptState {
    Pending,
    IssueBound {
        turn: TurnId,
        issue_lsn: Position,
    },
    Consumed {
        turn: TurnId,
        issue_lsn: Position,
        evidence_lsn: Position,
    },
    Invalidated {
        invalidation_lsn: Position,
        reason: RecoveryReceiptInvalidation,
    },
    Superseded {
        turn: TurnId,
        issue_lsn: Position,
        successor: FloorEpisode,
        barrier_lsn: Position,
    },
}

impl From<RecoveryReceiptState> for FloorReceiptState {
    fn from(value: RecoveryReceiptState) -> Self {
        match value {
            RecoveryReceiptState::Pending => Self::Pending,
            RecoveryReceiptState::IssueBound { turn, issue_lsn } => Self::IssueBound {
                turn,
                issue_lsn: Position(issue_lsn),
            },
            RecoveryReceiptState::Consumed {
                turn,
                issue_lsn,
                evidence_lsn,
            } => Self::Consumed {
                turn,
                issue_lsn: Position(issue_lsn),
                evidence_lsn: Position(evidence_lsn),
            },
            RecoveryReceiptState::Invalidated {
                invalidation_lsn,
                reason,
            } => Self::Invalidated {
                invalidation_lsn: Position(invalidation_lsn),
                reason,
            },
            RecoveryReceiptState::Superseded {
                turn,
                issue_lsn,
                successor,
                barrier_lsn,
            } => Self::Superseded {
                turn,
                issue_lsn: Position(issue_lsn),
                successor: successor.into(),
                barrier_lsn: Position(barrier_lsn),
            },
        }
    }
}

impl From<RecoveryReceiptView> for FloorReceipt {
    fn from(value: RecoveryReceiptView) -> Self {
        Self {
            request_id: value.request_id,
            claim: value.claim.into(),
            choice: value.choice,
            status: value.status.into(),
            attempt: FloorAttempt {
                spawn: value.attempt.spawn.map(Position),
                confirmation: value.attempt.confirmation.map(Position),
                end: value.attempt.end.map(Position),
            },
            prior_effects_unknown: value.prior_effects_unknown,
        }
    }
}

#[derive(Serialize)]
struct FloorPrefix {
    required: Option<Position>,
    resume: Option<Position>,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum FloorSubmission {
    Accepted {
        receipt: Box<FloorReceipt>,
    },
    Refused {
        reason: RecoverySubmitRefusal,
        prefix: FloorPrefix,
    },
    Unknown {
        request_id: Hex32,
    },
}

/// Encode the submission outcome, including any acknowledged prefix, without rounding positions.
///
/// # Errors
/// Returns an error if serialization fails or the response exceeds its byte bound.
pub fn encode_submission(result: &RecoverySubmitResult) -> Result<Vec<u8>> {
    let view = match result {
        RecoverySubmitResult::Accepted { receipt } => FloorSubmission::Accepted {
            receipt: Box::new((**receipt).into()),
        },
        RecoverySubmitResult::Refused { reason, prefix } => FloorSubmission::Refused {
            reason: *reason,
            prefix: FloorPrefix {
                required: prefix.required.map(Position),
                resume: prefix.resume.map(Position),
            },
        },
        RecoverySubmitResult::Unknown { request_id } => FloorSubmission::Unknown {
            request_id: *request_id,
        },
    };
    encode(&view)
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum FloorReceiptResult {
    Found { receipt: Box<FloorReceipt> },
    Absent { checked_through: Position },
    Unavailable { reason: RecoveryReadUnavailable },
}

/// Encode the historical lookup result with a lossless checked cut and receipt positions.
///
/// # Errors
/// Returns an error if serialization fails or the response exceeds its byte bound.
pub fn encode_receipt(result: &RecoveryReceiptResult) -> Result<Vec<u8>> {
    let view = match result {
        RecoveryReceiptResult::Found { receipt } => FloorReceiptResult::Found {
            receipt: Box::new((**receipt).into()),
        },
        RecoveryReceiptResult::Absent { checked_through } => FloorReceiptResult::Absent {
            checked_through: Position(*checked_through),
        },
        RecoveryReceiptResult::Unavailable { reason } => {
            FloorReceiptResult::Unavailable { reason: *reason }
        }
    };
    encode(&view)
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_RECOVERY_RESPONSE_BYTES {
        bail!("Floor recovery response is too large");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const AGENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const TURN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";
    const REQUEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const SUBMIT: &str = r#"{"request_id":"0000000000000000000000000000000000000000000000000000000000000000","expected":{"work":{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","goal":"01ARZ3NDEKTSV4RRFFQ69G5FAW","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX"},"issue_lsn":"20","source_lsn":"80","episode":{"work":{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","goal":"01ARZ3NDEKTSV4RRFFQ69G5FAW","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX"},"origin":"90"},"expected_predecessor":{"episode":{"work":{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","goal":"01ARZ3NDEKTSV4RRFFQ69G5FAW","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX"},"origin":"90"},"authorization_lsn":"300"},"control_lsn":"90","ticket":{"card":"01ARZ3NDEKTSV4RRFFQ69G5FAY","round":2}},"choice":"repeat_issued_fresh"}"#;
    const INPUT_POSITIONS: &[&str] = &[
        "/expected/issue_lsn",
        "/expected/source_lsn",
        "/expected/episode/origin",
        "/expected/expected_predecessor/episode/origin",
        "/expected/expected_predecessor/authorization_lsn",
        "/expected/control_lsn",
    ];
    const BOUNDARIES: &[(u64, &str)] = &[
        (0, "0"),
        (9_007_199_254_740_993, "9007199254740993"),
        (18_446_744_073_709_551_615, "18446744073709551615"),
    ];

    fn bytes(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    fn value(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).unwrap()
    }

    fn expected() -> ExpectedRecovery {
        decode_submit(SUBMIT.as_bytes()).unwrap().expected
    }

    #[test]
    fn submission_preserves_every_position_at_literal_u64_boundaries() {
        for &(number, text) in BOUNDARIES {
            let mut input: Value = serde_json::from_str(SUBMIT).unwrap();
            let mut output = input.clone();
            for pointer in INPUT_POSITIONS {
                *input.pointer_mut(pointer).unwrap() = json!(text);
                *output.pointer_mut(pointer).unwrap() = json!(number);
            }
            let parsed = decode_submit(&bytes(&input)).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), output);
        }
        let mut input: Value = serde_json::from_str(SUBMIT).unwrap();
        for key in ["episode", "expected_predecessor", "control_lsn", "ticket"] {
            input["expected"][key] = Value::Null;
        }
        let parsed = decode_submit(&bytes(&input)).unwrap();
        assert_eq!(parsed.expected.episode, None);
        assert_eq!(parsed.expected.expected_predecessor, None);
        assert_eq!(parsed.expected.control_lsn, None);
        assert_eq!(parsed.expected.ticket, None);
        assert_eq!(parsed.expected.issue_lsn, Lsn(20));
        assert_eq!(parsed.expected.source_lsn, Lsn(80));
    }

    #[test]
    fn submission_refuses_numeric_and_noncanonical_positions_at_every_nesting() {
        for pointer in INPUT_POSITIONS {
            for bad in [
                json!(0),
                json!(9_007_199_254_740_993_u64),
                json!(""),
                json!("00"),
                json!("01"),
                json!("+1"),
                json!("-1"),
                json!("1.0"),
                json!("1e0"),
                json!(" 1"),
                json!("1 "),
                json!("0:20"),
                json!("18446744073709551616"),
                json!(null),
                json!([]),
                json!({}),
            ] {
                if *pointer == "/expected/control_lsn" && bad.is_null() {
                    continue;
                }
                let mut input: Value = serde_json::from_str(SUBMIT).unwrap();
                *input.pointer_mut(pointer).unwrap() = bad;
                assert!(decode_submit(&bytes(&input)).is_err(), "{pointer}: {input}");
            }
        }
    }

    #[test]
    fn original_bytes_refuse_unknown_duplicate_and_escaped_duplicate_authority() {
        for (prefix, key, escaped, member) in [
            (
                "{",
                "request_id",
                r"request_\u0069d",
                format!("\"{REQUEST}\""),
            ),
            (
                r#""expected":{"#,
                "issue_lsn",
                r"issue_\u006csn",
                r#""20""#.into(),
            ),
            (
                r#""work":{"#,
                "agent",
                r"a\u0067ent",
                format!("\"{AGENT}\""),
            ),
            (r#""episode":{"#, "origin", r"ori\u0067in", r#""90""#.into()),
            (
                r#""episode":{"work":{"#,
                "agent",
                r"a\u0067ent",
                format!("\"{AGENT}\""),
            ),
            (
                r#""expected_predecessor":{"#,
                "authorization_lsn",
                r"authorization_\u006csn",
                r#""300""#.into(),
            ),
            (
                r#""expected_predecessor":{"episode":{"#,
                "origin",
                r"ori\u0067in",
                r#""90""#.into(),
            ),
            (
                r#""expected_predecessor":{"episode":{"work":{"#,
                "agent",
                r"a\u0067ent",
                format!("\"{AGENT}\""),
            ),
            (r#""ticket":{"#, "round", r"ro\u0075nd", "2".into()),
        ] {
            for added in [
                r#""extra":0"#.to_owned(),
                format!(r#""{key}":{member}"#),
                format!(r#""{escaped}":{member}"#),
            ] {
                let input = SUBMIT.replacen(prefix, &format!("{prefix}{added},"), 1);
                assert_ne!(input, SUBMIT);
                assert!(
                    decode_submit(input.as_bytes()).is_err(),
                    "{prefix}: {added}"
                );
            }
        }
    }

    #[test]
    fn every_authority_object_refuses_declaration_order_arrays() {
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
            let mut input: Value = serde_json::from_str(SUBMIT).unwrap();
            let object = input.pointer_mut(pointer).unwrap();
            *object = Value::Array(fields.iter().map(|field| object[*field].clone()).collect());
            assert!(decode_submit(&bytes(&input)).is_err(), "{pointer}");
        }
    }

    #[test]
    fn nullable_authority_fields_require_presence_and_ticket_round_stays_u32() {
        for key in ["episode", "expected_predecessor", "control_lsn", "ticket"] {
            let mut input: Value = serde_json::from_str(SUBMIT).unwrap();
            input["expected"].as_object_mut().unwrap().remove(key);
            assert!(decode_submit(&bytes(&input)).is_err(), "{key}");
        }
        for round in [json!(0), json!(4_294_967_295_u32)] {
            let mut input: Value = serde_json::from_str(SUBMIT).unwrap();
            input["expected"]["ticket"]["round"] = round.clone();
            let parsed = decode_submit(&bytes(&input)).unwrap();
            assert_eq!(json!(parsed.expected.ticket.unwrap().round), round);
        }
        for round in [json!(-1), json!(4_294_967_296_u64), json!("2")] {
            let mut input: Value = serde_json::from_str(SUBMIT).unwrap();
            input["expected"]["ticket"]["round"] = round;
            assert!(decode_submit(&bytes(&input)).is_err());
        }
    }

    #[test]
    fn inspection_and_receipt_queries_are_strict_original_objects() {
        let inspect = format!(r#"{{"agent":"{AGENT}"}}"#);
        let receipt = format!(r#"{{"request_id":"{REQUEST}"}}"#);
        assert_eq!(
            decode_inspect(inspect.as_bytes())
                .unwrap()
                .agent
                .to_string(),
            AGENT
        );
        assert_eq!(
            decode_receipt(receipt.as_bytes()).unwrap().request_id,
            Hex32::new([0; 32])
        );
        for (original, key, escaped, member) in [
            (inspect, "agent", r"a\u0067ent", AGENT),
            (receipt, "request_id", r"request_\u0069d", REQUEST),
        ] {
            for bad in [
                "{}".into(),
                format!(r#"["{member}"]"#),
                original.replacen('{', r#"{"extra":null,"#, 1),
                original.replacen('{', &format!(r#"{{"{key}":"{member}","#), 1),
                original.replacen('{', &format!(r#"{{"{escaped}":"{member}","#), 1),
            ] {
                let refused = if key == "agent" {
                    decode_inspect(bad.as_bytes()).is_err()
                } else {
                    decode_receipt(bad.as_bytes()).is_err()
                };
                assert!(refused, "{bad}");
            }
        }
    }

    #[test]
    fn http_bound_counts_original_whitespace_and_line_endings() {
        for original in [
            format!(r#"{{"agent":"{AGENT}"}}"#),
            format!(r#"{{"request_id":"{REQUEST}"}}"#),
            SUBMIT.to_owned(),
        ] {
            for ending in ["\n", "\r\n"] {
                let padded = format!(
                    "{}{}{ending}",
                    original,
                    " ".repeat(4096 - original.len() - ending.len())
                );
                assert_eq!(padded.len(), 4096);
                let accepts = |body: &[u8]| {
                    if original == SUBMIT {
                        decode_submit(body).is_ok()
                    } else if original.contains("agent") {
                        decode_inspect(body).is_ok()
                    } else {
                        decode_receipt(body).is_ok()
                    }
                };
                assert!(accepts(padded.as_bytes()));
                assert!(!accepts(format!("{padded} ").as_bytes()));
            }
        }
        for bad in [b"{} {}".as_slice(), b"\xff", b"null", b"[]"] {
            assert!(decode_submit(bad).is_err());
        }
    }

    #[test]
    fn inspection_encodes_full_values_and_every_nested_position_as_strings() {
        for &(number, text) in BOUNDARIES {
            let mut input: Value = serde_json::from_str(SUBMIT).unwrap();
            for pointer in INPUT_POSITIONS {
                *input.pointer_mut(pointer).unwrap() = json!(text);
            }
            let waiting = json!({"open_question":"waiting","budget":"ready","hops":"waiting","instruction_review":"ready","instruction_comparison":"waiting"});
            let result = RecoveryInspectResult::Available {
                through: Lsn(number),
                expected: Box::new(decode_submit(&bytes(&input)).unwrap().expected),
                reason: RecoveryReason::MissingSpawn,
                choices: RecoveryAllowedChoices::RepeatIssuedFresh,
                prior_effects_unknown: true,
                waiting: serde_json::from_value(waiting.clone()).unwrap(),
            };
            assert_eq!(
                value(&encode_inspection(&result).unwrap()),
                json!({
                    "state":"available", "through":text, "expected":input["expected"],
                    "reason":"missing_spawn", "choices":"repeat_issued_fresh",
                    "prior_effects_unknown":true,"waiting":waiting
                })
            );
        }
    }

    #[test]
    fn inspection_preserves_nullable_conditions_choices_and_waits() {
        let waiting = json!({"open_question":"ready","budget":"waiting","hops":"ready","instruction_review":"waiting","instruction_comparison":"ready"});
        let mut conditions: Value = serde_json::from_str(SUBMIT).unwrap();
        for key in ["episode", "expected_predecessor", "control_lsn", "ticket"] {
            conditions["expected"][key] = Value::Null;
        }
        for reason in [
            "held_or_paused_failure",
            "vanished",
            "unconfirmed_context",
            "missing_spawn",
        ] {
            for choices in ["fresh_only", "recorded_or_fresh", "repeat_issued_fresh"] {
                let result = RecoveryInspectResult::Available {
                    through: Lsn(0),
                    expected: Box::new(decode_submit(&bytes(&conditions)).unwrap().expected),
                    reason: serde_json::from_value(json!(reason)).unwrap(),
                    choices: serde_json::from_value(json!(choices)).unwrap(),
                    prior_effects_unknown: false,
                    waiting: serde_json::from_value(waiting.clone()).unwrap(),
                };
                assert_eq!(
                    value(&encode_inspection(&result).unwrap()),
                    json!({
                        "state":"available","through":"0","expected":conditions["expected"],
                        "reason":reason,"choices":choices,"prior_effects_unknown":false,"waiting":waiting
                    })
                );
            }
        }
        for choice in ["try_recorded", "fresh_with_recap", "repeat_issued_fresh"] {
            conditions["choice"] = json!(choice);
            let parsed = decode_submit(&bytes(&conditions)).unwrap();
            assert_eq!(serde_json::to_value(parsed.choice).unwrap(), json!(choice));
        }
    }

    fn receipt_pair(
        number: u64,
        text: &str,
        status: &Value,
        wire_status: &Value,
    ) -> (RecoveryReceiptView, Value) {
        let claim = expected().expected_predecessor.unwrap();
        let rpc = json!({
            "request_id":REQUEST,"claim":claim,"choice":"try_recorded","status":status,
            "attempt":{"spawn":number,"confirmation":number,"end":number},"prior_effects_unknown":true
        });
        let wire = json!({
            "request_id":REQUEST,"claim":{"episode":{"work":claim.episode.work,"origin":text},"authorization_lsn":text},
            "choice":"try_recorded","status":wire_status,
            "attempt":{"spawn":text,"confirmation":text,"end":text},"prior_effects_unknown":true
        });
        let mut receipt: RecoveryReceiptView = serde_json::from_value(rpc).unwrap();
        receipt.claim.episode.origin = Lsn(number);
        receipt.claim.authorization_lsn = Lsn(number);
        (receipt, wire)
    }

    #[test]
    fn receipts_preserve_all_lifecycle_variants_and_attempt_boundaries() {
        for &(number, text) in BOUNDARIES {
            let work = expected().work;
            for (status, wire_status) in [
                (json!({"state":"pending"}), json!({"state":"pending"})),
                (
                    json!({"state":"issue_bound","turn":TURN,"issue_lsn":number}),
                    json!({"state":"issue_bound","turn":TURN,"issue_lsn":text}),
                ),
                (
                    json!({"state":"consumed","turn":TURN,"issue_lsn":number,"evidence_lsn":number}),
                    json!({"state":"consumed","turn":TURN,"issue_lsn":text,"evidence_lsn":text}),
                ),
                (
                    json!({"state":"invalidated","invalidation_lsn":number,"reason":"goal_changed"}),
                    json!({"state":"invalidated","invalidation_lsn":text,"reason":"goal_changed"}),
                ),
                (
                    json!({"state":"superseded","turn":TURN,"issue_lsn":number,"successor":{"work":work,"origin":number},"barrier_lsn":number}),
                    json!({"state":"superseded","turn":TURN,"issue_lsn":text,"successor":{"work":work,"origin":text},"barrier_lsn":text}),
                ),
            ] {
                let (receipt, wire) = receipt_pair(number, text, &status, &wire_status);
                let found = encode_receipt(&RecoveryReceiptResult::Found {
                    receipt: Box::new(receipt),
                })
                .unwrap();
                assert_eq!(value(&found), json!({"state":"found","receipt":wire}));
                let accepted = encode_submission(&RecoverySubmitResult::Accepted {
                    receipt: Box::new(receipt),
                })
                .unwrap();
                assert_eq!(value(&accepted), json!({"state":"accepted","receipt":wire}));
                assert!(found.len() <= 4096);
                assert!(accepted.len() <= 4096);
            }
        }
        let (mut receipt, mut wire) = receipt_pair(
            0,
            "0",
            &json!({"state":"pending"}),
            &json!({"state":"pending"}),
        );
        receipt.attempt.spawn = None;
        receipt.attempt.confirmation = None;
        receipt.attempt.end = None;
        receipt.prior_effects_unknown = false;
        wire["attempt"] = json!({"spawn":null,"confirmation":null,"end":null});
        wire["prior_effects_unknown"] = json!(false);
        assert_eq!(
            value(
                &encode_receipt(&RecoveryReceiptResult::Found {
                    receipt: Box::new(receipt)
                })
                .unwrap()
            ),
            json!({"state":"found","receipt":wire})
        );
    }

    #[test]
    fn remaining_result_variants_preserve_fixed_vocabulary_and_nullable_prefixes() {
        for &(number, text) in BOUNDARIES {
            assert_eq!(
                value(
                    &encode_receipt(&RecoveryReceiptResult::Absent {
                        checked_through: Lsn(number)
                    })
                    .unwrap()
                ),
                json!({"state":"absent","checked_through":text})
            );
            for required in [None, Some(Lsn(number))] {
                for resume in [None, Some(Lsn(number))] {
                    let result = RecoverySubmitResult::Refused {
                        reason: RecoverySubmitRefusal::StalePredecessor,
                        prefix: dudes_proto::rpc::recovery::RecoveryKnownPrefix {
                            required,
                            resume,
                        },
                    };
                    assert_eq!(
                        value(&encode_submission(&result).unwrap()),
                        json!({"state":"refused","reason":"stale_predecessor","prefix":{"required":required.map(|_|text),"resume":resume.map(|_|text)}})
                    );
                }
            }
        }
        assert_eq!(
            value(
                &encode_submission(&RecoverySubmitResult::Unknown {
                    request_id: Hex32::new([0; 32])
                })
                .unwrap()
            ),
            json!({"state":"unknown","request_id":REQUEST})
        );
        for reason in [
            "agent_unavailable",
            "goal_unavailable",
            "halted",
            "turn_open",
            "not_recoverable",
            "claimed",
            "unavailable",
        ] {
            let expected = json!({"state":"unavailable","reason":reason});
            let result: RecoveryInspectResult = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(value(&encode_inspection(&result).unwrap()), expected);
        }
        for reason in [
            "journal_uncertain",
            "uncertified",
            "stale_cut",
            "invalid_history",
            "storage_unavailable",
        ] {
            let expected = json!({"state":"unavailable","reason":reason});
            let result: RecoveryReceiptResult = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(value(&encode_receipt(&result).unwrap()), expected);
        }
        for reason in [
            "request_conflict",
            "stale_work",
            "stale_source",
            "stale_episode",
            "stale_predecessor",
            "stale_control",
            "stale_ticket",
            "claimed",
            "choice_unavailable",
            "not_recoverable",
            "authority_unavailable",
            "unavailable",
        ] {
            let expected =
                json!({"state":"refused","reason":reason,"prefix":{"required":null,"resume":null}});
            let result: RecoverySubmitResult = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(value(&encode_submission(&result).unwrap()), expected);
        }
    }

    #[test]
    fn distinct_positions_keep_their_fields_across_input_and_output() {
        let mut input: Value = serde_json::from_str(SUBMIT).unwrap();
        input["expected"]["expected_predecessor"]["episode"]["origin"] = json!("91");
        input["expected"]["control_lsn"] = json!("92");
        let parsed = decode_submit(&bytes(&input)).unwrap();
        assert_eq!(parsed.expected.issue_lsn, Lsn(20));
        assert_eq!(parsed.expected.source_lsn, Lsn(80));
        assert_eq!(parsed.expected.episode.unwrap().origin, Lsn(90));
        assert_eq!(
            parsed.expected.expected_predecessor.unwrap().episode.origin,
            Lsn(91)
        );
        assert_eq!(
            parsed
                .expected
                .expected_predecessor
                .unwrap()
                .authorization_lsn,
            Lsn(300)
        );
        assert_eq!(parsed.expected.control_lsn, Some(Lsn(92)));
        let waiting = json!({"open_question":"ready","budget":"ready","hops":"ready","instruction_review":"ready","instruction_comparison":"ready"});
        let inspection = RecoveryInspectResult::Available {
            through: Lsn(97),
            expected: Box::new(parsed.expected),
            reason: RecoveryReason::Vanished,
            choices: RecoveryAllowedChoices::RecordedOrFresh,
            prior_effects_unknown: false,
            waiting: serde_json::from_value(waiting.clone()).unwrap(),
        };
        assert_eq!(
            value(&encode_inspection(&inspection).unwrap()),
            json!({
                "state":"available","through":"97","expected":input["expected"],"reason":"vanished",
                "choices":"recorded_or_fresh","prior_effects_unknown":false,"waiting":waiting
            })
        );
        let work = expected().work;
        let receipt: RecoveryReceiptView = serde_json::from_value(json!({
            "request_id":REQUEST,"claim":{"episode":{"work":work,"origin":11},"authorization_lsn":13},
            "choice":"fresh_with_recap","status":{"state":"superseded","turn":TURN,"issue_lsn":29,
                "successor":{"work":work,"origin":37},"barrier_lsn":41},
            "attempt":{"spawn":17,"confirmation":19,"end":23},"prior_effects_unknown":true
        })).unwrap();
        let wire = json!({
            "request_id":REQUEST,"claim":{"episode":{"work":work,"origin":"11"},"authorization_lsn":"13"},
            "choice":"fresh_with_recap","status":{"state":"superseded","turn":TURN,"issue_lsn":"29",
                "successor":{"work":work,"origin":"37"},"barrier_lsn":"41"},
            "attempt":{"spawn":"17","confirmation":"19","end":"23"},"prior_effects_unknown":true
        });
        assert_eq!(
            value(
                &encode_receipt(&RecoveryReceiptResult::Found {
                    receipt: Box::new(receipt)
                })
                .unwrap()
            ),
            json!({"state":"found","receipt":wire})
        );
        assert_eq!(
            value(
                &encode_submission(&RecoverySubmitResult::Refused {
                    reason: RecoverySubmitRefusal::StaleControl,
                    prefix: dudes_proto::rpc::recovery::RecoveryKnownPrefix {
                        required: Some(Lsn(43)),
                        resume: Some(Lsn(47))
                    }
                })
                .unwrap()
            ),
            json!({"state":"refused","reason":"stale_control","prefix":{"required":"43","resume":"47"}})
        );
    }

    #[test]
    fn receipt_encoders_preserve_every_choice_and_invalidation_reason() {
        let work = expected().work;
        for choice in ["try_recorded", "fresh_with_recap", "repeat_issued_fresh"] {
            for reason in [
                "goal_changed",
                "goal_completed",
                "ticket_changed",
                "lease_changed",
                "control_changed",
                "agent_unavailable",
                "authority_unavailable",
            ] {
                let receipt: RecoveryReceiptView = serde_json::from_value(json!({
                    "request_id":REQUEST,"claim":{"episode":{"work":work,"origin":11},"authorization_lsn":13},
                    "choice":choice,"status":{"state":"invalidated","invalidation_lsn":29,"reason":reason},
                    "attempt":{"spawn":null,"confirmation":null,"end":null},"prior_effects_unknown":false
                })).unwrap();
                let wire = json!({
                    "request_id":REQUEST,"claim":{"episode":{"work":work,"origin":"11"},"authorization_lsn":"13"},
                    "choice":choice,"status":{"state":"invalidated","invalidation_lsn":"29","reason":reason},
                    "attempt":{"spawn":null,"confirmation":null,"end":null},"prior_effects_unknown":false
                });
                assert_eq!(
                    value(
                        &encode_receipt(&RecoveryReceiptResult::Found {
                            receipt: Box::new(receipt)
                        })
                        .unwrap()
                    ),
                    json!({"state":"found","receipt":wire})
                );
                assert_eq!(
                    value(
                        &encode_submission(&RecoverySubmitResult::Accepted {
                            receipt: Box::new(receipt)
                        })
                        .unwrap()
                    ),
                    json!({"state":"accepted","receipt":wire})
                );
            }
        }
    }
}
