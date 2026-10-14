//! Floor RPC request and response contracts.
//!
//! The control-socket `floor.subscribe` and `floor.unsubscribe` contracts are declared in
//! `rpc/mod.rs` but remain unrouted, as checked by `bins/duded/tests/routed_methods.rs`.
//! The control-socket `floor.resync` notification comes from
//! `Machine::notify_floor_resync` in `crates/dudes-supervisor/src/machine.rs` after an isolation
//! leak is acknowledged. It is not an implemented queue-overflow notification.
//!
//! The browser stream has its own snapshot and delta frames. Its bounded queue closes on
//! overflow without sending `floor.resync`, through `QueueState::enqueue_frame` in
//! `bins/duded/src/floor_stream.rs` and the `QueueRead::Overflowed` arm in
//! `bins/duded/src/floor_gateway.rs`. Browser `floor.resync` frames serve other events, including
//! enrollment and local-day changes, through `FloorStreams` in `floor_stream.rs`.

use std::fmt;

use dudes_types::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::record::Hex32;

/// Largest `base_url` value in UTF-8 bytes.
pub const SESSION_BASE_URL_MAX_BYTES: usize = 2048;
/// Length of the unpadded base64url bootstrap bearer.
pub const SESSION_BOOTSTRAP_LEN: usize = 43;
/// Canonical unpadded base64url encoding of exactly 32 bytes.
pub const SESSION_BOOTSTRAP_PATTERN: &str = "^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$";
/// Lifetime of a bootstrap bearer, in seconds.
pub const SESSION_EXPIRES_IN_S: u64 = 60;

const INVALID_BASE_URL: &str = "invalid floor session base_url: expected at most 2048 UTF-8 bytes";
const INVALID_BOOTSTRAP: &str = "invalid floor session bootstrap: expected canonical 43-character unpadded base64url encoding of 32 bytes";
const INVALID_EXPIRY: &str = "invalid floor session expiry: expected 60 seconds";

/// `floor.session.issue` result.
///
/// The bootstrap bearer remains a string on the wire, but [`Debug`](fmt::Debug) redacts it so a
/// diagnostic cannot disclose the capability. Deserialization enforces every finite field and
/// returns value-free errors; result objects remain open to additive fields.
#[derive(PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionIssueResult {
    /// The daemon's configured origin, at most 2,048 UTF-8 bytes.
    #[serde(deserialize_with = "deserialize_base_url")]
    pub base_url: String,
    /// A single-use bearer: canonical unpadded base64url for exactly 32 bytes.
    #[serde(deserialize_with = "deserialize_bootstrap")]
    #[schemars(length(equal = SESSION_BOOTSTRAP_LEN), regex(pattern = SESSION_BOOTSTRAP_PATTERN))]
    pub bootstrap: String,
    /// The bearer lifetime. Exactly 60 seconds.
    #[serde(deserialize_with = "deserialize_expiry")]
    #[schemars(range(min = SESSION_EXPIRES_IN_S, max = SESSION_EXPIRES_IN_S))]
    pub expires_in_s: u64,
}

impl fmt::Debug for SessionIssueResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionIssueResult")
            .field("base_url", &self.base_url)
            .field("bootstrap", &"<redacted>")
            .field("expires_in_s", &self.expires_in_s)
            .finish()
    }
}

/// Floor sign-ins one daemon run hands to the next: `floor.session.export`'s result, and what
/// `floor.session.import` takes in its params.
///
/// A digest is not a cookie and cannot be presented as one. [`Debug`](fmt::Debug) names only the
/// count, so a diagnostic never lists the digests.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionExport {
    /// The run that exported them.
    pub run: String,
    /// The Floor origin that run served; empty when it served none.
    pub origin: String,
    /// The live sessions at the time of the export.
    pub sessions: Vec<ExportedSession>,
}

impl fmt::Debug for SessionExport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionExport")
            .field("run", &self.run)
            .field("origin", &self.origin)
            .field("sessions", &self.sessions.len())
            .finish()
    }
}

/// One Floor sign-in as the exporting run held it.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExportedSession {
    /// SHA-256 of the session cookie.
    #[schemars(with = "String")]
    pub digest: Hex32,
    /// When the person signed in. The eight-hour limit keeps counting from here.
    pub created: Timestamp,
    /// When the session was last used, not refreshed by the export.
    pub last_used: Timestamp,
    /// Tickets the session created in the run.
    pub ticket_creates: u32,
    /// Tickets the session assigned in the run.
    pub ticket_assigns: u32,
    /// Checks the session started in the run.
    pub check_runs: u32,
}

impl fmt::Debug for ExportedSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ExportedSession(<digest withheld>)")
    }
}

/// `floor.session.import` params.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionImportParams {
    /// What an earlier run exported.
    pub exported: SessionExport,
}

/// `floor.session.import` result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionImportResult {
    /// How many of the exported sessions this run admitted.
    pub restored: u32,
}

/// A SHA-256 digest as the wire writes it: 64 lowercase hexadecimal digits.
pub const SHA256_PATTERN: &str = "^[0-9a-f]{64}$";

const INVALID_SHA256: &str = "invalid sha256: expected 64 lowercase hexadecimal digits";

/// `floor.enrollment.reload` params. With no `apply` the call is a check: it reports
/// the file and the change and applies nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentReloadParams {
    /// Apply the change only when the file and the change are exactly these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply: Option<EnrollmentApply>,
}

/// The file and the change a caller expects a reload to apply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentApply {
    /// The SHA-256 of the file bytes the caller expects the daemon to read.
    #[serde(deserialize_with = "deserialize_sha256")]
    #[schemars(regex(pattern = SHA256_PATTERN))]
    pub sha256: String,
    /// Workspace ids the caller expects to be added.
    pub added: Vec<String>,
    /// Workspace ids the caller expects to be removed.
    pub removed: Vec<String>,
    /// Workspace ids the caller expects to be changed.
    pub changed: Vec<String>,
}

/// Why an `apply` was declined. A declined `apply` changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentRefusal {
    /// The bytes the daemon read do not hash to `apply.sha256`.
    FileChanged,
    /// The change differs from the ids `apply` named.
    UnexpectedChanges,
    /// A reload that read the file later has already been applied.
    Superseded,
}

/// One workspace whose entry differs, and every differing key by its TOML name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangedWorkspace {
    /// The workspace id.
    pub id: String,
    /// Each differing key, by its TOML name.
    pub fields: Vec<String>,
}

/// `floor.enrollment.reload` result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EnrollmentReloadResult {
    /// True only when this call replaced the running enrollment.
    pub applied: bool,
    /// Present when an `apply` was declined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<EnrollmentRefusal>,
    /// The SHA-256 of the bytes the daemon read.
    #[schemars(regex(pattern = SHA256_PATTERN))]
    pub sha256: String,
    /// The file the daemon read; absent when its path is not valid UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The workspace ids running after the call, in admission order.
    pub workspaces: Vec<String>,
    /// Ids the file adds, sorted.
    pub added: Vec<String>,
    /// Ids the file removes, sorted.
    pub removed: Vec<String>,
    /// Entries the file changes, sorted by id.
    pub changed: Vec<ChangedWorkspace>,
    /// Other sections whose value differs from the running one; each still needs a restart.
    pub not_applied: Vec<String>,
}

fn deserialize_sha256<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(INVALID_SHA256))
    }
}

fn deserialize_base_url<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.len() <= SESSION_BASE_URL_MAX_BYTES {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(INVALID_BASE_URL))
    }
}

fn deserialize_bootstrap<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if canonical_bootstrap(&value) {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(INVALID_BOOTSTRAP))
    }
}

fn deserialize_expiry<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = u64::deserialize(deserializer)?;
    if value == SESSION_EXPIRES_IN_S {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(INVALID_EXPIRY))
    }
}

fn canonical_bootstrap(value: &str) -> bool {
    super::secret::well_formed(value, SESSION_BOOTSTRAP_LEN)
        && matches!(
            value.as_bytes().last().copied(),
            Some(
                b'A' | b'E'
                    | b'I'
                    | b'M'
                    | b'Q'
                    | b'U'
                    | b'Y'
                    | b'c'
                    | b'g'
                    | b'k'
                    | b'o'
                    | b's'
                    | b'w'
                    | b'0'
                    | b'4'
                    | b'8'
            )
        )
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn wire(base_url: &str, bootstrap: &str, expires_in_s: u64) -> Value {
        json!({
            "base_url": base_url,
            "bootstrap": bootstrap,
            "expires_in_s": expires_in_s,
            "future_result_field": "ignored",
        })
    }

    fn assert_schema() {
        let schema = schemars::schema_for!(SessionIssueResult);
        let properties = &schema.as_value()["properties"];
        assert_eq!(properties["base_url"]["type"], "string");
        assert!(
            properties["base_url"].get("maxLength").is_none(),
            "JSON Schema length counts characters, so it cannot restate the UTF-8 byte bound"
        );
        assert_eq!(properties["bootstrap"]["minLength"], SESSION_BOOTSTRAP_LEN);
        assert_eq!(properties["bootstrap"]["maxLength"], SESSION_BOOTSTRAP_LEN);
        assert_eq!(
            properties["bootstrap"]["pattern"],
            SESSION_BOOTSTRAP_PATTERN
        );
        assert_eq!(properties["expires_in_s"]["minimum"], SESSION_EXPIRES_IN_S);
        assert_eq!(properties["expires_in_s"]["maximum"], SESSION_EXPIRES_IN_S);
    }

    /// The session-issue result has three finite fields, and the bearer stays out of
    /// diagnostics. The byte, canonical-encoding and fixed-expiry controls each differ from a
    /// valid value in only the property they test.
    #[test]
    fn session_issue_result_round_trips_and_refuses_out_of_range_fields() {
        let base_url = "é".repeat(1024);
        assert_eq!(base_url.len(), SESSION_BASE_URL_MAX_BYTES);
        assert_eq!(
            base_url.chars().count(),
            SESSION_BASE_URL_MAX_BYTES / 2,
            "control: the accepted boundary distinguishes UTF-8 bytes from characters"
        );
        let sentinel = format!("zq7{}", "A".repeat(40));
        assert_eq!(sentinel.len(), SESSION_BOOTSTRAP_LEN);

        let result: SessionIssueResult =
            serde_json::from_value(wire(&base_url, &sentinel, SESSION_EXPIRES_IN_S))
                .expect("the finite result decodes and ignores future result fields");
        assert_eq!(result.base_url, base_url);
        assert_eq!(result.bootstrap, sentinel);
        assert_eq!(result.expires_in_s, SESSION_EXPIRES_IN_S);

        let json = serde_json::to_value(&result).expect("the result encodes");
        assert_eq!(json["base_url"], result.base_url);
        assert_eq!(json["bootstrap"], result.bootstrap);
        assert_eq!(json["expires_in_s"], SESSION_EXPIRES_IN_S);
        assert!(
            serde_json::to_string(&result).unwrap().contains(&sentinel),
            "the bearer remains the exact JSON wire value"
        );
        let debug = format!("{result:?}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(
            !debug.contains(&sentinel),
            "Debug must not disclose the bootstrap bearer: {debug}"
        );

        let overlong = format!("url-sentinel-{}", "é".repeat(1025));
        let error = serde_json::from_value::<SessionIssueResult>(wire(
            &overlong,
            &sentinel,
            SESSION_EXPIRES_IN_S,
        ))
        .unwrap_err();
        assert_eq!(error.to_string(), INVALID_BASE_URL);
        assert!(!error.to_string().contains(&overlong));

        for noncanonical_last in ['B', 'C', 'D'] {
            let noncanonical = format!("{}{noncanonical_last}", "A".repeat(42));
            assert_eq!(noncanonical.len(), SESSION_BOOTSTRAP_LEN);
            let error = serde_json::from_value::<SessionIssueResult>(wire(
                "http://127.0.0.1",
                &noncanonical,
                SESSION_EXPIRES_IN_S,
            ))
            .unwrap_err();
            assert_eq!(error.to_string(), INVALID_BOOTSTRAP);
            assert!(!error.to_string().contains(&noncanonical));
        }
        for invalid in [
            "A".repeat(42),
            "A".repeat(44),
            format!("{}+", "A".repeat(42)),
        ] {
            let error = serde_json::from_value::<SessionIssueResult>(wire(
                "http://127.0.0.1",
                &invalid,
                SESSION_EXPIRES_IN_S,
            ))
            .unwrap_err();
            assert_eq!(error.to_string(), INVALID_BOOTSTRAP);
            assert!(!error.to_string().contains(&invalid));
        }
        for canonical in ["A".repeat(43), format!("{}E", "A".repeat(42))] {
            assert!(
                serde_json::from_value::<SessionIssueResult>(wire(
                    "http://127.0.0.1",
                    &canonical,
                    SESSION_EXPIRES_IN_S,
                ))
                .is_ok(),
                "control: more than one canonical final sextet is accepted"
            );
        }

        for expires_in_s in [0, SESSION_EXPIRES_IN_S - 1, SESSION_EXPIRES_IN_S + 1] {
            let error = serde_json::from_value::<SessionIssueResult>(wire(
                "http://127.0.0.1",
                &sentinel,
                expires_in_s,
            ))
            .unwrap_err();
            assert_eq!(error.to_string(), INVALID_EXPIRY);
        }

        assert_schema();
    }
}

const FLOOR_LSN_PATTERN: &str = concat!(
    "^(0|[1-9][0-9]{0,18}",
    "|1[0-7][0-9]{18}",
    "|18[0-3][0-9]{17}",
    "|184[0-3][0-9]{16}",
    "|1844[0-5][0-9]{15}",
    "|18446[0-6][0-9]{14}",
    "|184467[0-3][0-9]{13}",
    "|1844674[0-3][0-9]{12}",
    "|184467440[0-6][0-9]{10}",
    "|1844674407[0-2][0-9]{9}",
    "|18446744073[0-6][0-9]{8}",
    "|1844674407370[0-8][0-9]{6}",
    "|18446744073709[0-4][0-9]{5}",
    "|184467440737095[0-4][0-9]{4}",
    "|18446744073709550[0-9]{3}",
    "|18446744073709551[0-5][0-9]{2}",
    "|1844674407370955160[0-9]",
    "|1844674407370955161[0-4]",
    "|18446744073709551615",
    ")$(?![\\s\\S])",
);

/// The daemon's count of goals first finished, with an observed pass, on its own local calendar day.
/// A sibling of `floor.snapshot` and `floor.delta`; a present delta replaces the whole
/// value, and an absent sibling means the daemon does not support it, never a count of zero.
///
/// `local_date` and `utc_offset_minutes` are the daemon host's at the instant it answered, so a
/// browser in another timezone can say whose day it is showing and never converts it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FloorFinishedToday {
    /// The aggregate was read at one projection cut.
    Ready {
        /// `YYYY-MM-DD`, the daemon host's local calendar date.
        #[schemars(length(equal = 10), regex(pattern = FINISHED_TODAY_DATE_PATTERN))]
        local_date: String,
        /// The daemon host's offset from UTC at that instant, in minutes east.
        #[schemars(range(min = -1560, max = 1560))]
        utc_offset_minutes: i32,
        /// Goals whose first observed completion falls in that day, as canonical unsigned decimal
        /// text, with no cap.
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String", length(min = 1, max = 20), regex(pattern = FLOOR_LSN_PATTERN))]
        total: u64,
        /// The journal position the total was read at, as canonical unsigned decimal text.
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String", length(min = 1, max = 20), regex(pattern = FLOOR_LSN_PATTERN))]
        through_lsn: u64,
    },
    /// The daemon cannot give a complete total now: the host zone is unusable, the date is not
    /// representable, or a completion could not be resolved. Never rendered as zero.
    Unavailable,
}

/// A `local_date` is four digits, a hyphen, two digits, a hyphen and two digits.
pub const FINISHED_TODAY_DATE_PATTERN: &str = "^[0-9]{4}-[0-9]{2}-[0-9]{2}$";

pub(crate) mod decimal_u64 {
    use serde::{Deserialize, Deserializer, Serializer};

    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's `with` hands the field over by reference"
    )]
    pub(crate) fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse::<u64>()
            .ok()
            .filter(|value| value.to_string() == text)
            .ok_or_else(|| serde::de::Error::custom("expected canonical unsigned decimal text"))
    }
}

/// The failed append position disclosed to an authenticated Floor session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct FloorJournalStopped {
    /// Attempted physical journal position, encoded as canonical unsigned decimal text.
    #[serde(deserialize_with = "deserialize_floor_lsn")]
    #[schemars(
        with = "String",
        length(min = 1, max = 20),
        regex(pattern = FLOOR_LSN_PATTERN)
    )]
    pub lsn: dudes_types::Lsn,
}

impl Serialize for FloorJournalStopped {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct as _;
        let mut stopped = serializer.serialize_struct("FloorJournalStopped", 1)?;
        stopped.serialize_field("lsn", &self.lsn.0.to_string())?;
        stopped.end()
    }
}

fn deserialize_floor_lsn<'de, D>(deserializer: D) -> Result<dudes_types::Lsn, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    value
        .parse::<u64>()
        .ok()
        .filter(|lsn| lsn.to_string() == value)
        .map(dudes_types::Lsn)
        .ok_or_else(|| serde::de::Error::custom("invalid stopped-journal lsn"))
}

/// Bounded read-only configuration metadata beside a browser Floor snapshot.
///
/// This and its parts are `#[non_exhaustive]`, so a later field is not a breaking change. Build
/// one from [`Default`] and set its fields.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SettingsView {
    /// Effective switches for this daemon run.
    pub switches: SettingsSwitches,
    /// Admission ceilings and reserves for this daemon run.
    pub budget: SettingsBudget,
    /// Registered adapters, independent of executable availability.
    pub providers: Vec<dudes_types::Provider>,
    /// All operator-enrolled workspaces, independent of browser Hire eligibility.
    pub workspaces: Vec<SettingsWorkspace>,
    /// Daemon release and browser wire identity.
    pub about: SettingsAbout,
}

/// The finite Floor switch allowlist.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the explicit read-only wire reports eight configured switch facts"
)]
pub struct SettingsSwitches {
    /// Browser question answering.
    pub answer: bool,
    /// Browser hire.
    pub hire: bool,
    /// Browser pause, resume and rename.
    pub control: bool,
    /// Browser goal text changes.
    pub goal: bool,
    /// Browser renewal of a goal that has nothing left, from its workspace's enrolled allowance.
    pub renew: bool,
    /// Browser retirement.
    pub retire: bool,
    /// Browser terminal attachment.
    pub terminal: bool,
    /// Browser Git tab reads of an agent's checkout.
    pub git: bool,
}

/// Dollar amounts are decimal micro-dollar strings and token amounts are decimal strings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SettingsBudget {
    /// Floor daily dollar ceiling, with absence distinct from zero.
    pub floor_usd_per_day_micros: Option<String>,
    /// Dollars reserved per turn while cost is unseen.
    pub per_turn_usd_micros: String,
    /// Tokens reserved per turn while usage is unseen.
    pub per_turn_tokens: String,
}

/// Explicit safe display fields from one enrolled workspace.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SettingsWorkspace {
    /// Opaque operator-enrolled identifier.
    pub workspace_id: String,
    /// Explicit non-secret display label, never derived from a path.
    pub label: String,
    /// Agent daily dollar ceiling.
    pub agent_usd_per_day_micros: Option<String>,
    /// Agent daily token ceiling.
    pub agent_tokens_per_day: Option<String>,
    /// Initial goal dollar ceiling.
    pub goal_usd_micros: Option<String>,
    /// Initial goal token ceiling.
    pub goal_tokens: Option<String>,
}

/// Versions reported by the addressed daemon.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SettingsAbout {
    /// Daemon release version.
    pub daemon_version: String,
    /// Version of the browser snapshot protocol.
    pub browser_wire_version: u32,
}

#[cfg(test)]
mod stopped_tests {
    use super::FloorJournalStopped;
    use dudes_types::Lsn;
    use serde_json::json;

    #[test]
    fn stopped_journal_lsn_is_canonical_exact_and_bounded() {
        let schema = crate::schema::schemas()
            .into_iter()
            .find(|doc| doc.file_name == "rpc.schema.json")
            .unwrap()
            .schema;
        let validator = jsonschema::validator_for(&schema["$defs"]["FloorJournalStopped"]).unwrap();
        for lsn in [
            0,
            1,
            9_007_199_254_740_993,
            10_000_000_000_000_000_000,
            u64::MAX - 1,
            u64::MAX,
        ] {
            let value = FloorJournalStopped { lsn: Lsn(lsn) };
            let wire = serde_json::to_value(value).unwrap();
            assert_eq!(wire, json!({"lsn": lsn.to_string()}));
            assert!(validator.is_valid(&wire));
            assert_eq!(
                serde_json::from_value::<FloorJournalStopped>(wire).unwrap(),
                value
            );
            assert!(
                serde_json::to_vec(&json!({"journal_stopped": value}))
                    .unwrap()
                    .len()
                    < 64
            );
        }
        for invalid in [
            json!(7),
            json!("00"),
            json!("01"),
            json!("-1"),
            json!("+1"),
            json!("1e2"),
            json!("1.0"),
            json!(" 1"),
            json!("1\n"),
            json!("1\r\n"),
            json!("18446744073709551616"),
            json!("99999999999999999999"),
            json!("100000000000000000000"),
        ] {
            let invalid = json!({"lsn": invalid});
            assert!(!validator.is_valid(&invalid), "schema accepted {invalid}");
            let error = serde_json::from_value::<FloorJournalStopped>(invalid).unwrap_err();
            assert!(!error.to_string().contains("18446744073709551616"));
        }
        assert_eq!(
            schema["$defs"]["FloorJournalStopped"]["properties"]["lsn"]["type"],
            "string"
        );
        assert_eq!(
            schema["$defs"]["FloorJournalStopped"]["properties"]["lsn"]["maxLength"],
            20
        );
    }
}

#[cfg(test)]
mod finished_today_tests {
    use super::FloorFinishedToday;
    use serde_json::json;

    fn validator() -> jsonschema::Validator {
        let schema = crate::schema::schemas()
            .into_iter()
            .find(|doc| doc.file_name == "rpc.schema.json")
            .unwrap()
            .schema;
        jsonschema::validator_for(&schema["$defs"]["FloorFinishedToday"]).unwrap()
    }

    #[test]
    fn ready_and_unavailable_have_exactly_the_decided_wire_shape() {
        let validator = validator();
        let ready = FloorFinishedToday::Ready {
            local_date: "2026-10-07".to_owned(),
            utc_offset_minutes: 780,
            total: u64::MAX,
            through_lsn: 9_007_199_254_740_993,
        };
        let wire = serde_json::to_value(&ready).unwrap();
        assert_eq!(
            wire,
            json!({
                "state": "ready",
                "local_date": "2026-10-07",
                "utc_offset_minutes": 780,
                "total": "18446744073709551615",
                "through_lsn": "9007199254740993",
            })
        );
        assert!(validator.is_valid(&wire));
        assert_eq!(
            serde_json::from_value::<FloorFinishedToday>(wire.clone()).unwrap(),
            ready
        );
        assert!(serde_json::to_vec(&wire).unwrap().len() <= 256);
        let wire = serde_json::to_value(FloorFinishedToday::Unavailable).unwrap();
        assert_eq!(wire, json!({"state": "unavailable"}));
        assert!(validator.is_valid(&wire));
    }

    #[test]
    fn a_total_that_is_not_canonical_decimal_text_is_refused() {
        let validator = validator();
        let base = json!({
            "state": "ready", "local_date": "2026-10-07", "utc_offset_minutes": -660,
            "total": "3", "through_lsn": "9",
        });
        assert!(validator.is_valid(&base));
        for total in [
            json!(3),
            json!("03"),
            json!("-1"),
            json!("18446744073709551616"),
            json!(""),
        ] {
            let mut wire = base.clone();
            wire["total"] = total;
            assert!(!validator.is_valid(&wire), "{wire}");
            assert!(serde_json::from_value::<FloorFinishedToday>(wire).is_err());
        }
        for date in ["2026-1-07", "2026-10-07T00", "20261007"] {
            let mut wire = base.clone();
            wire["local_date"] = json!(date);
            assert!(!validator.is_valid(&wire), "{wire}");
        }
    }
}

/// The most bytes the `checks` sibling of a Floor snapshot may serialize to. An enrollment whose
/// sibling would be larger is refused when it loads and when it reloads.
pub const MAX_CHECKS_BYTES: usize = 16_384;

/// The `checks` sibling of a Floor snapshot: each enrolled workspace's completion checks, as Hire
/// and the goal form offer them. No command, timeout or directory crosses it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChecksView {
    /// Every enrolled workspace, in id order.
    pub workspaces: Vec<WorkspaceChecks>,
}

/// One enrolled workspace's checks, its own check first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceChecks {
    /// The workspace's opaque id.
    pub workspace_id: String,
    /// Its checks, in the order the operator file lists them.
    pub checks: Vec<CheckView>,
}

/// One enrolled check, by name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckView {
    /// Its id, unique within its workspace.
    pub id: String,
    /// Its display label.
    pub label: String,
    /// SHA-256 over its id, label, command and timeout, which a request that names it presents.
    #[schemars(with = "String")]
    pub revision: crate::record::Hex32,
    /// The operator declared that it may already pass, so a goal with it does not run it first.
    pub may_already_pass: bool,
}

impl ChecksView {
    /// Whether this sibling serializes within [`MAX_CHECKS_BYTES`].
    #[must_use]
    pub fn fits(&self) -> bool {
        serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= MAX_CHECKS_BYTES)
    }
}
