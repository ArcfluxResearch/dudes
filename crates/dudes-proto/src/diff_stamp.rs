//! Bounded metadata a ticket round records about its review baseline and post-check sample.
//!
//! Neither payload holds file text or a per-file hash. Each encodes to at most
//! [`MAX_STAMP_PAYLOAD_BYTES`], and a constructor that cannot fit its fields refuses instead of
//! shortening them.

use std::fmt;

use dudes_types::{AgentId, CardId, GitCommit, GoalId, Lsn, Timestamp};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::diff::DiffRefusal;

/// Hard cap on one encoded baseline or stamp payload.
pub const MAX_STAMP_PAYLOAD_BYTES: usize = 1024;
/// Encoding revision written into every payload.
pub const STAMP_PAYLOAD_VERSION: u32 = 1;

/// Why a payload could not be built or read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StampPayloadError {
    /// The encoding is longer than [`MAX_STAMP_PAYLOAD_BYTES`].
    TooLarge {
        /// The encoded length.
        len: usize,
    },
    /// A digest field is not 64 lowercase hexadecimal characters.
    InvalidDigest,
    /// The capture interval ends before it starts.
    InvalidInterval,
    /// The bytes are not a payload of this encoding.
    Malformed,
}

impl fmt::Display for StampPayloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { len } => write!(
                f,
                "stamp payload is {len} bytes, over the {MAX_STAMP_PAYLOAD_BYTES} byte cap"
            ),
            Self::InvalidDigest => f.write_str("digest is not 64 lowercase hexadecimal characters"),
            Self::InvalidInterval => f.write_str("capture interval ends before it starts"),
            Self::Malformed => f.write_str("bytes are not a stamp payload"),
        }
    }
}

impl std::error::Error for StampPayloadError {}

/// The ticket round a payload belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketBinding {
    /// Ticket identity.
    pub card: CardId,
    /// Installed round.
    pub round: u32,
    /// Installed goal.
    pub goal: GoalId,
    /// Claimant when the capture began.
    pub claimant: AgentId,
}

/// When a capture began and ended, in Unix milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CaptureInterval {
    /// Capture start.
    pub started_at: Timestamp,
    /// Capture end.
    pub ended_at: Timestamp,
}

/// Sampler and text-policy revisions in force for a capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SamplerVersions {
    /// Candidate sampling algorithm revision.
    pub sampler: u32,
    /// Text and sensitive-content policy revision.
    pub policy: u32,
}

/// A SHA-256 digest in lowercase hexadecimal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DigestHex([u8; 64]);

impl DigestHex {
    /// Accept exactly 64 lowercase hexadecimal characters.
    ///
    /// # Errors
    /// Refuses any other text.
    pub fn parse(text: &str) -> Result<Self, StampPayloadError> {
        let bytes: [u8; 64] = text
            .as_bytes()
            .try_into()
            .map_err(|_| StampPayloadError::InvalidDigest)?;
        if bytes
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        {
            Ok(Self(bytes))
        } else {
            Err(StampPayloadError::InvalidDigest)
        }
    }

    /// The digest text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("validated ASCII")
    }
}

impl JsonSchema for DigestHex {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "DigestHex".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "pattern": "^[0-9a-f]{64}$"})
    }
}

impl Serialize for DigestHex {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DigestHex {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// What the round-start observation found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BaselineOutcome {
    /// The root and start commit were observed.
    Captured {
        /// Filesystem device of the opened root.
        device: u64,
        /// Directory inode of the opened root.
        inode: u64,
        /// Full start commit.
        head: GitCommit,
        /// Tracked changes existed when the round started.
        preexisting_tracked: bool,
    },
    /// No baseline could be observed.
    Unavailable {
        /// Finite reason.
        reason: DiffRefusal,
    },
}

/// What the post-check sample found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StampOutcome {
    /// The scope was sampled. An incomplete sample is never compared for freshness.
    Sampled {
        /// The whole eligible scope was sampled without a cap or refusal.
        complete: bool,
        /// Scope digest.
        digest: DigestHex,
        /// Omitted eligible pairs.
        omitted: u32,
        /// The omitted count is only a lower bound.
        omitted_lower_bound: bool,
        /// Purely untracked names.
        untracked: u32,
        /// The untracked count is only a lower bound.
        untracked_lower_bound: bool,
    },
    /// No sample could be taken.
    Unavailable {
        /// Finite reason.
        reason: DiffRefusal,
    },
}

/// The round-start observation, recorded before the round's first turn is issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BaselinePayload {
    /// Encoding revision.
    pub v: u32,
    /// Ticket round.
    #[serde(flatten)]
    pub binding: TicketBinding,
    /// Capture interval.
    #[serde(flatten)]
    pub interval: CaptureInterval,
    /// Sampler and policy revisions.
    pub versions: SamplerVersions,
    /// Observation result.
    pub outcome: BaselineOutcome,
}

/// The post-check scope sample, recorded after the predicate result is appended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StampPayload {
    /// Encoding revision.
    pub v: u32,
    /// Ticket round.
    #[serde(flatten)]
    pub binding: TicketBinding,
    /// Position of the baseline record this sample compares against.
    pub baseline_lsn: Lsn,
    /// Position of the predicate result this sample follows.
    pub check_lsn: Lsn,
    /// Capture interval.
    #[serde(flatten)]
    pub interval: CaptureInterval,
    /// Sampler and policy revisions.
    pub versions: SamplerVersions,
    /// Sample result.
    pub outcome: StampOutcome,
}

/// Encoded bytes known to fit the cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StampBytes(Vec<u8>);

impl StampBytes {
    /// Encode `value` as compact JSON.
    ///
    /// # Errors
    /// Refuses an encoding over [`MAX_STAMP_PAYLOAD_BYTES`].
    pub fn encode<T: Serialize>(value: &T) -> Result<Self, StampPayloadError> {
        Self::from_vec(serde_json::to_vec(value).map_err(|_| StampPayloadError::Malformed)?)
    }

    /// Accept already-encoded bytes.
    ///
    /// # Errors
    /// Refuses bytes over [`MAX_STAMP_PAYLOAD_BYTES`].
    pub fn from_vec(bytes: Vec<u8>) -> Result<Self, StampPayloadError> {
        if bytes.len() > MAX_STAMP_PAYLOAD_BYTES {
            return Err(StampPayloadError::TooLarge { len: bytes.len() });
        }
        Ok(Self(bytes))
    }

    /// The encoded bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, StampPayloadError> {
    if bytes.len() > MAX_STAMP_PAYLOAD_BYTES {
        return Err(StampPayloadError::TooLarge { len: bytes.len() });
    }
    serde_json::from_slice(bytes).map_err(|_| StampPayloadError::Malformed)
}

fn check_interval(interval: CaptureInterval) -> Result<(), StampPayloadError> {
    if interval.ended_at < interval.started_at {
        return Err(StampPayloadError::InvalidInterval);
    }
    Ok(())
}

impl BaselinePayload {
    /// Build a payload that fits the cap.
    ///
    /// # Errors
    /// Refuses a reversed interval or an encoding over the cap.
    pub fn new(
        binding: TicketBinding,
        interval: CaptureInterval,
        versions: SamplerVersions,
        outcome: BaselineOutcome,
    ) -> Result<Self, StampPayloadError> {
        check_interval(interval)?;
        let payload = Self {
            v: STAMP_PAYLOAD_VERSION,
            binding,
            interval,
            versions,
            outcome,
        };
        payload.encode()?;
        Ok(payload)
    }

    /// The bounded encoding.
    ///
    /// # Errors
    /// Refuses an encoding over the cap.
    pub fn encode(&self) -> Result<StampBytes, StampPayloadError> {
        StampBytes::encode(self)
    }

    /// Read a recorded payload.
    ///
    /// # Errors
    /// Refuses oversize or malformed bytes and a reversed interval.
    pub fn decode(bytes: &[u8]) -> Result<Self, StampPayloadError> {
        let payload: Self = decode(bytes)?;
        check_interval(payload.interval)?;
        Ok(payload)
    }
}

impl StampPayload {
    /// Build a payload that fits the cap.
    ///
    /// # Errors
    /// Refuses a reversed interval or an encoding over the cap.
    pub fn new(
        binding: TicketBinding,
        baseline_lsn: Lsn,
        check_lsn: Lsn,
        interval: CaptureInterval,
        versions: SamplerVersions,
        outcome: StampOutcome,
    ) -> Result<Self, StampPayloadError> {
        check_interval(interval)?;
        let payload = Self {
            v: STAMP_PAYLOAD_VERSION,
            binding,
            baseline_lsn,
            check_lsn,
            interval,
            versions,
            outcome,
        };
        payload.encode()?;
        Ok(payload)
    }

    /// The bounded encoding.
    ///
    /// # Errors
    /// Refuses an encoding over the cap.
    pub fn encode(&self) -> Result<StampBytes, StampPayloadError> {
        StampBytes::encode(self)
    }

    /// Read a recorded payload.
    ///
    /// # Errors
    /// Refuses oversize or malformed bytes and a reversed interval.
    pub fn decode(bytes: &[u8]) -> Result<Self, StampPayloadError> {
        let payload: Self = decode(bytes)?;
        check_interval(payload.interval)?;
        Ok(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> TicketBinding {
        TicketBinding {
            card: CardId::NIL,
            round: 2,
            goal: GoalId::NIL,
            claimant: AgentId::NIL,
        }
    }

    fn interval() -> CaptureInterval {
        CaptureInterval {
            started_at: Timestamp::from_unix_millis(1_000),
            ended_at: Timestamp::from_unix_millis(1_250),
        }
    }

    const VERSIONS: SamplerVersions = SamplerVersions {
        sampler: 1,
        policy: 3,
    };

    fn commit() -> GitCommit {
        GitCommit::parse("0123456789abcdef0123456789abcdef01234567").unwrap()
    }

    fn text(bytes: &StampBytes) -> &str {
        std::str::from_utf8(bytes.as_slice()).unwrap()
    }

    #[test]
    fn baseline_encodes_to_the_literal_text() {
        let payload = BaselinePayload::new(
            binding(),
            interval(),
            VERSIONS,
            BaselineOutcome::Captured {
                device: 16_777_231,
                inode: 4_242,
                head: commit(),
                preexisting_tracked: true,
            },
        )
        .unwrap();
        let bytes = payload.encode().unwrap();
        assert_eq!(
            text(&bytes),
            concat!(
                r#"{"v":1,"card":"00000000000000000000000000","round":2,"#,
                r#""goal":"00000000000000000000000000","claimant":"00000000000000000000000000","#,
                r#""started_at":1000,"ended_at":1250,"versions":{"sampler":1,"policy":3},"#,
                r#""outcome":{"state":"captured","device":16777231,"inode":4242,"#,
                r#""head":"0123456789abcdef0123456789abcdef01234567","preexisting_tracked":true}}"#,
            )
        );
        assert_eq!(BaselinePayload::decode(bytes.as_slice()).unwrap(), payload);
    }

    #[test]
    fn stamp_encodes_to_the_literal_text() {
        let payload = StampPayload::new(
            binding(),
            Lsn(12),
            Lsn(77),
            interval(),
            VERSIONS,
            StampOutcome::Sampled {
                complete: false,
                digest: DigestHex::parse(&"ab".repeat(32)).unwrap(),
                omitted: 3,
                omitted_lower_bound: true,
                untracked: 5,
                untracked_lower_bound: false,
            },
        )
        .unwrap();
        let bytes = payload.encode().unwrap();
        assert_eq!(
            text(&bytes),
            concat!(
                r#"{"v":1,"card":"00000000000000000000000000","round":2,"#,
                r#""goal":"00000000000000000000000000","claimant":"00000000000000000000000000","#,
                r#""baseline_lsn":12,"check_lsn":77,"started_at":1000,"ended_at":1250,"#,
                r#""versions":{"sampler":1,"policy":3},"#,
                r#""outcome":{"state":"sampled","complete":false,"#,
                r#""digest":"abababababababababababababababababababababababababababababababab","#,
                r#""omitted":3,"omitted_lower_bound":true,"untracked":5,"#,
                r#""untracked_lower_bound":false}}"#,
            )
        );
        assert_eq!(StampPayload::decode(bytes.as_slice()).unwrap(), payload);
    }

    #[test]
    fn unavailable_outcomes_name_a_finite_reason() {
        let payload = BaselinePayload::new(
            binding(),
            interval(),
            VERSIONS,
            BaselineOutcome::Unavailable {
                reason: DiffRefusal::TimedOut,
            },
        )
        .unwrap();
        let bytes = payload.encode().unwrap();
        assert!(
            text(&bytes).ends_with(r#""outcome":{"state":"unavailable","reason":"timed_out"}}"#),
            "{}",
            text(&bytes)
        );
    }

    #[test]
    fn worst_case_payloads_fit_with_room_to_spare() {
        let worst_binding = TicketBinding {
            card: CardId::parse("7ZZZZZZZZZZZZZZZZZZZZZZZZZ").unwrap(),
            round: u32::MAX,
            goal: GoalId::parse("7ZZZZZZZZZZZZZZZZZZZZZZZZZ").unwrap(),
            claimant: AgentId::parse("7ZZZZZZZZZZZZZZZZZZZZZZZZZ").unwrap(),
        };
        let worst_interval = CaptureInterval {
            started_at: Timestamp::from_unix_millis(i64::MIN),
            ended_at: Timestamp::from_unix_millis(i64::MAX),
        };
        let worst_versions = SamplerVersions {
            sampler: u32::MAX,
            policy: u32::MAX,
        };
        let baseline = BaselinePayload::new(
            worst_binding,
            worst_interval,
            worst_versions,
            BaselineOutcome::Captured {
                device: u64::MAX,
                inode: u64::MAX,
                head: commit(),
                preexisting_tracked: false,
            },
        )
        .unwrap();
        assert_eq!(baseline.encode().unwrap().as_slice().len(), 425);
        let stamp = StampPayload::new(
            worst_binding,
            Lsn(u64::MAX),
            Lsn(u64::MAX),
            worst_interval,
            worst_versions,
            StampOutcome::Sampled {
                complete: false,
                digest: DigestHex::parse(&"f".repeat(64)).unwrap(),
                omitted: u32::MAX,
                omitted_lower_bound: false,
                untracked: u32::MAX,
                untracked_lower_bound: false,
            },
        )
        .unwrap();
        assert_eq!(stamp.encode().unwrap().as_slice().len(), 551);
    }

    #[test]
    fn the_cap_is_exact_and_refuses_rather_than_truncates() {
        #[derive(Serialize)]
        struct Padded(String);
        // A JSON string adds two quote bytes.
        let at_cap = Padded("x".repeat(1022));
        assert_eq!(StampBytes::encode(&at_cap).unwrap().as_slice().len(), 1024);
        let over = Padded("x".repeat(1023));
        assert_eq!(
            StampBytes::encode(&over),
            Err(StampPayloadError::TooLarge { len: 1025 })
        );
        assert_eq!(
            BaselinePayload::decode(&[b' '; 1025]),
            Err(StampPayloadError::TooLarge { len: 1025 })
        );
    }

    #[test]
    fn digests_must_be_sixty_four_lowercase_hex_characters() {
        for bad in [
            String::new(),
            "0".repeat(32),
            "A".repeat(64),
            "g".repeat(64),
            "a".repeat(65),
        ] {
            assert_eq!(
                DigestHex::parse(&bad),
                Err(StampPayloadError::InvalidDigest)
            );
        }
        assert!(DigestHex::parse(&"0".repeat(64)).is_ok());
        let upper = format!(r#""{}""#, "A".repeat(64));
        assert!(serde_json::from_str::<DigestHex>(&upper).is_err());
    }

    #[test]
    fn a_reversed_interval_is_refused() {
        let reversed = CaptureInterval {
            started_at: Timestamp::from_unix_millis(2),
            ended_at: Timestamp::from_unix_millis(1),
        };
        assert_eq!(
            BaselinePayload::new(
                binding(),
                reversed,
                VERSIONS,
                BaselineOutcome::Unavailable {
                    reason: DiffRefusal::Unreadable
                }
            ),
            Err(StampPayloadError::InvalidInterval)
        );
    }
}
