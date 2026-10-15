//! Bounded, display-only checkout review responses. These bytes are never a verdict.

use dudes_types::{AgentId, CardId, GoalId, Lsn, Seq, Timestamp};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Maximum UTF-8 bytes on one file side before JSON escaping.
pub const MAX_DIFF_SIDE_BYTES: usize = 16 * 1024;
/// Maximum bytes on one source line before JSON escaping.
pub const MAX_DIFF_LINE_BYTES: usize = 4 * 1024;
/// Maximum complete files in a content response.
pub const MAX_DIFF_FILES: usize = 8;
/// Maximum entries in one selection page.
pub const MAX_DIFF_PAGE_ENTRIES: usize = 64;
/// Maximum candidates retained by one manifest.
pub const MAX_DIFF_CANDIDATES: usize = 256;
/// Maximum serialized metadata in a retained manifest.
pub const MAX_DIFF_MANIFEST_BYTES: usize = 128 * 1024;
/// Maximum serialized page response, including its envelope.
pub const MAX_DIFF_PAGE_BYTES: usize = 64 * 1024;
/// Maximum serialized content response, including its envelope.
pub const MAX_DIFF_CONTENT_BYTES: usize = 128 * 1024;
/// Maximum repository-relative display path bytes. Longer names receive no token.
pub const MAX_DIFF_PATH_BYTES: usize = 200;
/// Manifest lifetime in milliseconds. Reads never renew it.
pub const DIFF_MANIFEST_LIFETIME_MS: i64 = 60_000;

/// A finite refusal. A refused response contains no repository data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiffRefusal {
    /// The recorded baseline is absent or its objects cannot be read.
    BaselineUnavailable,
    /// The current ticket no longer has the requested binding.
    StaleTicket,
    /// The manifest or selection no longer belongs to this live session.
    StaleManifest,
    /// The manifest's fixed lifetime ended.
    Expired,
    /// The request is not one of the admitted closed shapes.
    InvalidRequest,
    /// Either content switch is disabled.
    Disabled,
    /// Another capture is running or the session interval has not elapsed.
    Busy,
    /// The session has spent its cumulative response allowance.
    SessionLimit,
    /// This platform cannot perform the descriptor-relative read.
    UnsupportedPlatform,
    /// The recorded checkout layout disagrees with the opened directories.
    WorktreeMismatch,
    /// The repository uses unsupported object or index storage.
    UnsupportedStorage,
    /// The recorded root has been replaced or linked elsewhere.
    RootMismatch,
    /// The read reached its deadline or was cancelled.
    TimedOut,
    /// No suitable daemon-resolved Git program is available.
    GitUnavailable,
    /// A finite filesystem or Git refusal.
    Unreadable,
}

/// Why a whole file pair has no source preview.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiffOmissionReason {
    /// A side contains NUL bytes.
    Binary,
    /// A side has invalid UTF-8 or forbidden control or format characters.
    UnsupportedText,
    /// A path matches the sensitive-path policy.
    SensitivePath,
    /// A side matches a versioned best-effort secret recognizer.
    SensitiveContent,
    /// A whole side exceeds the byte limit.
    OversizeFile,
    /// A whole source line exceeds the byte limit.
    OversizeLine,
    /// A side is a symlink, gitlink, directory, or other special file.
    UnsupportedKind,
    /// A file or its directory entry changed while being sampled.
    Raced,
    /// A regular file or object could not be read.
    Unreadable,
    /// This complete file would exceed the serialized response limit.
    ResponseLimit,
}

/// The comparison's change kind. Renames require an exact bounded identity match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiffKind {
    /// Absent at baseline and tracked in the current index.
    Added,
    /// Present on both sides with changed bytes or mode.
    Modified,
    /// Present at baseline and absent from the current checkout.
    Deleted,
    /// An old and new path share an exact blob identity.
    Renamed,
    /// At least one side has an unsupported file kind.
    TypeChanged,
}

/// Whether the bounded complete sample matches the recorded post-check sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiffFreshness {
    /// Matches the review sample taken after this check.
    MatchesReviewSample,
    /// The reviewed checkout content changed after this check.
    ChangedAfterCheck,
    /// Missing, incomplete, raced, or incompatible sampling observations.
    Unknown,
}

/// The exact ticket and recorded target a response belongs to. It contains no pathname.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffBinding {
    /// Ticket identity.
    pub card: CardId,
    /// Current card sequence expected by the request.
    pub seq: Seq,
    /// Installed ticket round.
    pub round: u32,
    /// Installed goal.
    pub goal: GoalId,
    /// Current recorded claimant.
    pub claimant: AgentId,
    /// The actual durable predicate-result position, when one exists.
    #[serde(with = "optional_lsn")]
    pub check_lsn: Option<Lsn>,
    /// Opaque fingerprint of the round baseline.
    pub baseline_id: String,
    /// Opaque fingerprint of the recorded target and baseline root identity.
    pub target_id: String,
    /// Candidate sampling algorithm version.
    pub sampler_version: u32,
    /// Text and sensitive-content policy version.
    pub policy_version: u32,
    /// Tracked changes existed when the round started.
    pub preexisting_tracked: bool,
}

/// A metadata row's content eligibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum DiffAvailability {
    /// This row has a server-minted selection token.
    Available,
    /// This whole pair cannot be previewed.
    Unavailable {
        /// Finite omission reason.
        reason: DiffOmissionReason,
    },
}

/// One stable metadata row. Display paths never become browser request selectors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffEntry {
    /// Opaque selection token, absent for unavailable rows.
    pub entry_id: Option<String>,
    /// Baseline path for an exact rename, otherwise absent.
    pub old_path: Option<String>,
    /// Repository-relative display path.
    pub path: String,
    /// Bounded comparison kind.
    pub kind: DiffKind,
    /// Whether a token may select this row.
    pub availability: DiffAvailability,
}

/// A page of one immutable candidate manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffPage {
    /// Opaque manifest identity.
    pub manifest_id: String,
    /// Binding rechecked by the caller before and after sampling.
    pub binding: DiffBinding,
    /// Fixed expiry in Unix milliseconds.
    pub expires_at: Timestamp,
    /// Complete entries within the count and serialized-byte caps.
    pub entries: Vec<DiffEntry>,
    /// Opaque continuation within this cached candidate set.
    pub next_cursor: Option<String>,
    /// Candidate rows omitted from the cache.
    pub omitted: u32,
    /// Enumeration stopped, so `omitted` is only a lower bound.
    pub omitted_lower_bound: bool,
    /// Purely untracked names are counted without being disclosed.
    pub untracked: u32,
    /// Untracked counting stopped before completion.
    pub untracked_lower_bound: bool,
    /// Enumeration or cache limits omitted candidates.
    pub truncated: bool,
    /// A bounded sample association, never a predicate verdict.
    pub freshness: DiffFreshness,
}

/// One complete admitted text pair. A missing side is the empty string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffFile {
    /// Opaque token from the manifest.
    pub entry_id: String,
    /// Old rename path, otherwise absent.
    pub old_path: Option<String>,
    /// Repository-relative display path.
    pub path: String,
    /// Comparison kind.
    pub kind: DiffKind,
    /// Complete baseline text.
    pub old: String,
    /// Complete current text.
    pub current: String,
}

/// A selected pair omitted whole from a content response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffOmission {
    /// Selected token.
    pub entry_id: String,
    /// Finite whole-pair refusal.
    pub reason: DiffOmissionReason,
}

/// Selected complete text pairs and explicit whole-pair omissions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffContent {
    /// Opaque manifest identity.
    pub manifest_id: String,
    /// Current response binding.
    pub binding: DiffBinding,
    /// Manifest's original fixed expiry.
    pub expires_at: Timestamp,
    /// Bounded sample comparison.
    pub freshness: DiffFreshness,
    /// Complete files in manifest order.
    pub files: Vec<DiffFile>,
    /// Selected pairs that cannot be shown.
    pub omitted: Vec<DiffOmission>,
    /// Serialized-byte or file-count caps omitted a selected pair.
    pub truncated: bool,
}

/// A closed response mode. Existing metadata contracts remain separate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum DiffResponse {
    /// First page of a newly minted manifest.
    Manifest(DiffPage),
    /// A continuation of the immutable cached manifest.
    Page(DiffPage),
    /// Selected eligible complete file pairs.
    Content(DiffContent),
    /// A finite response containing no new repository data.
    Refused {
        /// Refusal reason.
        reason: DiffRefusal,
    },
}

mod optional_lsn {
    use dudes_types::Lsn;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    #[expect(
        clippy::ref_option,
        reason = "serde field serializers borrow the stored Option"
    )]
    pub(super) fn serialize<S: Serializer>(
        value: &Option<Lsn>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_some(&value.0.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Lsn>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|text| {
                if text.is_empty()
                    || (text.len() > 1 && text.starts_with('0'))
                    || !text.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(D::Error::custom("invalid log position"));
                }
                text.parse::<u64>()
                    .map(Lsn)
                    .map_err(|_| D::Error::custom("invalid log position"))
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_positions_use_lossless_canonical_decimal_strings() {
        let mut binding = DiffBinding {
            card: CardId::NIL,
            seq: Seq(1),
            round: 1,
            goal: GoalId::NIL,
            claimant: AgentId::NIL,
            check_lsn: None,
            baseline_id: "a".repeat(64),
            target_id: "b".repeat(64),
            sampler_version: 1,
            policy_version: 1,
            preexisting_tracked: false,
        };
        for position in [0, 9_007_199_254_740_993, u64::MAX] {
            binding.check_lsn = Some(Lsn(position));
            let value = serde_json::to_value(&binding).unwrap();
            assert_eq!(value["check_lsn"], position.to_string());
            assert_eq!(
                serde_json::from_value::<DiffBinding>(value).unwrap(),
                binding
            );
        }
        for invalid in [
            serde_json::json!(12),
            serde_json::json!("01"),
            serde_json::json!("+1"),
            serde_json::json!("18446744073709551616"),
        ] {
            let mut value = serde_json::to_value(&binding).unwrap();
            value["check_lsn"] = invalid;
            assert!(serde_json::from_value::<DiffBinding>(value).is_err());
        }
    }
}
