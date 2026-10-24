//! The Floor's opt-in pause, stop and resume of one agent: the bounded request, its request
//! digest, the replayable control index, and the finite outcomes the three routes may report.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context as _, Result, anyhow, bail};
use dudes_journal::Reader;
use dudes_proto::Record;
use dudes_proto::record::Hex32;
use dudes_types::{AgentId, Lsn};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};

/// Most bytes one pause or resume request body may carry, checked before a byte of it is read.
/// The largest canonical body is far below it, so a `413` refuses only a non-minimal encoding.
pub const MAX_CONTROL_BODY_BYTES: usize = 512;

/// The `control` sibling a Floor snapshot carries exactly when control is enabled: an empty object,
/// outside `FloorProjection` and `floor.delta`, and discarded with the rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ControlView {}

/// The one value [`ControlView`] takes.
pub const CONTROL_VIEW: ControlView = ControlView {};

/// Which of the three routes a request arrived on. The route is the verb: no request member selects
/// one, and there is no fourth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlVerb {
    /// `POST /floor/pause`.
    Pause,
    /// `POST /floor/halt`: stop the agent's turn and hold it until the Floor lifts its own halt.
    Halt,
    /// `POST /floor/resume`.
    Resume,
}

impl ControlVerb {
    /// The digest domain: one id can never apply on both routes.
    const fn domain(self) -> &'static [u8] {
        match self {
            Self::Pause => b"dudes.pause.v1\0",
            Self::Halt => b"dudes.halt.v1\0",
            Self::Resume => b"dudes.resume.v1\0",
        }
    }

    /// The `state` an applied request reports.
    pub(crate) const fn applied(self) -> &'static str {
        match self {
            Self::Pause => "paused",
            Self::Halt => "halted",
            Self::Resume => "resumed",
        }
    }
}

/// One canonical, bounded pause or resume request. Nothing in it selects an effect beyond the
/// route's own verb for the one agent it names.
#[derive(Clone)]
pub struct ControlRequest {
    pub(crate) verb: ControlVerb,
    pub(crate) request_id: Hex32,
    pub(crate) request_wire: String,
    pub(crate) request_digest: Hex32,
    pub(crate) agent: AgentId,
    pub(crate) control_lsn: Option<Lsn>,
}

impl fmt::Debug for ControlRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ControlRequest(<redacted>)")
    }
}

/// What an admitted pause, stop or resume may truthfully report once the writer has run it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlOutcome {
    /// This request's attributed record is durable and indexed, on the first attempt or on a
    /// same-key retry. The standing cause was set or lifted; no turn is promised either way.
    Applied {
        /// The request identity as the page sent it.
        request_id: String,
        /// The route the durable record came from.
        verb: ControlVerb,
        /// The agent it names.
        agent: AgentId,
        /// That record's position: the agent's control identity it set.
        control_lsn: Lsn,
    },
    /// No effect: the first reason, in a fixed order, that holds.
    Refused {
        /// The request identity as the page sent it.
        request_id: String,
        /// `agent_absent`, `agent_attached`, `agent_halted`, `stale_control` or `no_running_turn`.
        reason: &'static str,
    },
}

/// An empty HTTP refusal with no invented identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlRefusal {
    /// The session expired or was revoked before the append permit; nothing was appended.
    Forbidden,
    /// The same request identity names a different request digest.
    Conflict,
    /// This request's own append may be durable; the same identity may be checked again later.
    Unavailable,
    /// This request appended nothing and may be retried at once with the same identity.
    UnavailableNoEffect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlJson {
    request_id: String,
    agent: String,
    // A custom decoder, so an absent member is refused rather than read as `null`: the derived
    // decoder treats a missing `Option` field as `None`, and the request must carry exactly three members.
    #[serde(deserialize_with = "string_or_null")]
    control_lsn: Option<String>,
}

/// A JSON string or `null`; a number, or anything else, is refused.
fn string_or_null<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

impl ControlRequest {
    /// Decode one body, for the route that received it.
    ///
    /// One flat object with exactly three members: an unknown member is refused by
    /// `deny_unknown_fields`, a duplicate by the derived decoder, a missing one by the decoders
    /// above, and a nested value by the string type of every member — all before any effect.
    pub(crate) fn parse(verb: ControlVerb, bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_CONTROL_BODY_BYTES {
            bail!("Floor control request is too large");
        }
        // The derived decoder also reads a struct from a positional array; only an object is a
        // valid request body.
        if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
            bail!("Floor control request is not one JSON object");
        }
        let raw: ControlJson =
            serde_json::from_slice(bytes).context("invalid Floor control request JSON")?;
        let request_id = crate::floor_actions::decode_attempt(&raw.request_id)
            .context("invalid Floor control request id")?;
        let agent =
            AgentId::parse(&raw.agent).map_err(|_| anyhow!("invalid Floor control agent"))?;
        let control_lsn = raw
            .control_lsn
            .as_deref()
            .map(crate::floor_answer::decode_canonical_lsn)
            .transpose()
            .context("invalid Floor control_lsn")?;
        Ok(Self {
            verb,
            request_id,
            request_digest: request_digest(
                verb,
                &raw.agent,
                raw.control_lsn.as_deref().unwrap_or(""),
            ),
            request_wire: raw.request_id,
            agent,
            control_lsn,
        })
    }
}

/// SHA-256 over the route's domain (`dudes.pause.v1\0`, `dudes.halt.v1\0` or `dudes.resume.v1\0`), then `agent` and
/// `control_lsn`, each framed like every Floor request digest; a `null` control identity enters
/// with zero length. The request identity does not enter it.
fn request_digest(verb: ControlVerb, agent: &str, control_lsn: &str) -> Hex32 {
    let mut hasher = Sha256::new();
    hasher.update(verb.domain());
    for (field, value) in [("agent", agent), ("control_lsn", control_lsn)] {
        hasher.update(
            u16::try_from(field.len())
                .expect("static field name")
                .to_be_bytes(),
        );
        hasher.update(field.as_bytes());
        hasher.update(
            u32::try_from(value.len())
                .expect("bounded field")
                .to_be_bytes(),
        );
        hasher.update(value.as_bytes());
    }
    Hex32::new(hasher.finalize().into())
}

/// One durable browser pause or resume, as the index keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IndexedControl {
    pub(crate) request_digest: Hex32,
    pub(crate) verb: ControlVerb,
    pub(crate) agent: AgentId,
    pub(crate) lsn: Lsn,
}

/// The replayable control index: request identity to what the durable record said. Only journal
/// records may change it, and only after their fsync.
#[derive(Debug, Default)]
pub(crate) struct ControlIndex {
    controls: BTreeMap<Hex32, IndexedControl>,
}

impl ControlIndex {
    pub(crate) fn replay(reader: &Reader) -> Result<Self> {
        let mut index = Self::default();
        for item in reader
            .iter_from(Lsn::ZERO)
            .context("reading the Floor control journal index")?
        {
            let (lsn, entry) = item.context("reading a Floor control journal entry")?;
            index.apply(lsn, &entry.record)?;
        }
        Ok(index)
    }

    pub(crate) fn apply(&mut self, lsn: Lsn, record: &Record) -> Result<()> {
        let (verb, agent, attribution) = match record {
            Record::Pause {
                agent,
                browser: Some(attribution),
            } => (ControlVerb::Pause, agent, attribution),
            Record::Resume {
                agent,
                browser: Some(attribution),
            } => (ControlVerb::Resume, agent, attribution),
            Record::BrowserHalt {
                agent,
                browser: attribution,
                ..
            } => (ControlVerb::Halt, agent, attribution),
            _ => return Ok(()),
        };
        if self.controls.contains_key(&attribution.request_id) {
            bail!("duplicate Floor control request in journal");
        }
        self.controls.insert(
            attribution.request_id,
            IndexedControl {
                request_digest: attribution.request_digest,
                verb,
                agent: *agent,
                lsn,
            },
        );
        Ok(())
    }

    pub(crate) fn get(&self, request_id: Hex32) -> Option<IndexedControl> {
        self.controls.get(&request_id).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUEST_ID: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const AGENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn body(control_lsn: &serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "request_id": REQUEST_ID,
            "agent": AGENT,
            "control_lsn": control_lsn,
        }))
        .unwrap()
    }

    fn hex(digest: Hex32) -> serde_json::Value {
        serde_json::to_value(digest).unwrap()
    }

    /// The digest is pinned to vectors computed outside Rust (Python's hashlib over the same
    /// framing): the two routes' domains differ, `null` enters with zero length, and the request
    /// identity does not enter it.
    #[test]
    fn each_route_has_one_domain_separated_request_digest() {
        let pause = ControlRequest::parse(ControlVerb::Pause, &body(&"1099511628288".into()))
            .expect("canonical");
        assert_eq!(pause.control_lsn, Some(Lsn(1_099_511_628_288)));
        assert_eq!(pause.request_id.bytes(), [0; 32]);
        assert_eq!(
            hex(pause.request_digest),
            "792bd23bbecff2a54d76d6af927ad4af7688e31a7eb10bd1dab4c08c915a0ae9"
        );
        let resume = ControlRequest::parse(ControlVerb::Resume, &body(&"1099511628288".into()))
            .expect("canonical");
        assert_eq!(
            hex(resume.request_digest),
            "f67c5219baf9d9db1099b2b49c44d7917aeed371557b813f8cda041130a55dd9"
        );
        let null = ControlRequest::parse(ControlVerb::Pause, &body(&serde_json::Value::Null))
            .expect("null");
        assert_eq!(null.control_lsn, None);
        assert_eq!(
            hex(null.request_digest),
            "e8d28eca29a1ef43a56340a53237bd03002a55cd5eb49f3e8ac3df56d0923b03"
        );
        let other_id = serde_json::to_vec(&serde_json::json!({
            "request_id": "BAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "agent": AGENT,
            "control_lsn": "1099511628288",
        }))
        .unwrap();
        assert_eq!(
            ControlRequest::parse(ControlVerb::Pause, &other_id)
                .unwrap()
                .request_digest,
            pause.request_digest
        );
    }

    /// Exactly three members: an absent `control_lsn` is refused, not read as `null`; a number
    /// is refused, not rounded; and the control, `null`, decodes.
    #[test]
    fn control_lsn_is_a_present_string_or_null() {
        let missing = serde_json::to_vec(&serde_json::json!({
            "request_id": REQUEST_ID,
            "agent": AGENT,
        }))
        .unwrap();
        assert!(ControlRequest::parse(ControlVerb::Pause, &missing).is_err());
        assert!(ControlRequest::parse(ControlVerb::Pause, &body(&7.into())).is_err());
        assert!(ControlRequest::parse(ControlVerb::Pause, &body(&"0".into())).is_err());
        assert!(ControlRequest::parse(ControlVerb::Pause, &body(&serde_json::Value::Null)).is_ok());
    }

    /// One flat object and nothing else: the derived decoder would read the same three values
    /// from a positional array, so the array is refused first.
    #[test]
    fn only_an_object_is_a_request() {
        let array = serde_json::to_vec(&serde_json::json!([REQUEST_ID, AGENT, null])).unwrap();
        assert!(ControlRequest::parse(ControlVerb::Pause, &array).is_err());
        let spaced = [b" \n".as_slice(), &body(&serde_json::Value::Null)].concat();
        assert!(
            ControlRequest::parse(ControlVerb::Pause, &spaced).is_ok(),
            "control: leading whitespace before the object"
        );
    }
}
