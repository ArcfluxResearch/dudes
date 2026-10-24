//! A bounded browser model change and its journal-derived retry truth.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context as _, Result, anyhow, bail};
use dudes_journal::Reader;
use dudes_proto::Record;
use dudes_proto::record::Hex32;
use dudes_types::{AgentId, Lsn, ModelName};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// Maximum encoded body: two 64-byte names, an agent id and a request id, with room for escaping.
pub const MAX_MODEL_BODY_BYTES: usize = 1_024;

/// Canonical fields admitted before the writer receives them.
#[derive(Clone)]
pub struct ModelRequest {
    pub(crate) request_id: Hex32,
    pub(crate) request_wire: String,
    pub(crate) request_id_digest: Hex32,
    pub(crate) request_digest: Hex32,
    pub(crate) agent: AgentId,
    /// The override the page showed when the person chose, where `None` is no override.
    pub(crate) expected: Option<ModelName>,
    /// The override to set, where `None` clears it.
    pub(crate) model: Option<ModelName>,
}

impl fmt::Debug for ModelRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ModelRequest(<redacted>)")
    }
}

/// The durable change path's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelOutcome {
    /// This request's original change is durable and indexed, or the override already held it.
    Changed {
        /// Browser request identity.
        request_id: String,
        /// Structural agent identity.
        agent: AgentId,
        /// The override this request set, or `None` for a clear.
        model: Option<ModelName>,
    },
    /// No record was appended.
    Refused {
        /// Browser request identity.
        request_id: String,
        /// The refusal's name.
        reason: &'static str,
    },
}

/// An empty HTTP refusal with no invented outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelRefusal {
    /// The admitting session no longer authorizes append.
    Forbidden,
    /// This request identity already names different canonical fields.
    Conflict,
    /// Durability is uncertain. Keep this exact attempt.
    Unavailable,
    /// Nothing was appended by this request.
    UnavailableNoEffect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelJson {
    request_id: String,
    agent: String,
    expected: Option<String>,
    model: Option<String>,
}

fn shaped(text: Option<&str>, what: &'static str) -> Result<Option<ModelName>> {
    text.map(|text| ModelName::parse(text).map_err(|_| anyhow!("invalid Floor model {what}")))
        .transpose()
}

impl ModelRequest {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_MODEL_BODY_BYTES {
            bail!("Floor model request is too large");
        }
        if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
            bail!("Floor model request is not one JSON object");
        }
        let object: serde_json::Value =
            serde_json::from_slice(bytes).context("invalid Floor model JSON")?;
        let Some(fields) = object.as_object() else {
            bail!("Floor model request is not one JSON object");
        };
        if !fields.contains_key("expected") || !fields.contains_key("model") {
            bail!("Floor model request omits the model fields");
        }
        let raw: ModelJson = serde_json::from_slice(bytes).context("invalid Floor model fields")?;
        let decoded = crate::floor_actions::decode_attempt(&raw.request_id)
            .context("invalid Floor model request id")?;
        let agent = AgentId::parse(&raw.agent).map_err(|_| anyhow!("invalid Floor model agent"))?;
        let expected = shaped(raw.expected.as_deref(), "expectation")?;
        let model = shaped(raw.model.as_deref(), "name")?;
        let mut hasher = Sha256::new();
        hasher.update(b"dudes.model.v1\0");
        for (field, value) in [
            ("agent", agent.to_string()),
            (
                "expected",
                expected
                    .as_ref()
                    .map_or_else(String::new, ToString::to_string),
            ),
            (
                "model",
                model.as_ref().map_or_else(String::new, ToString::to_string),
            ),
        ] {
            hasher.update(
                u16::try_from(field.len())
                    .expect("static field")
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
        Ok(Self {
            request_id: decoded,
            request_wire: raw.request_id,
            request_id_digest: Hex32::new(Sha256::digest(decoded.bytes()).into()),
            request_digest: Hex32::new(hasher.finalize().into()),
            agent,
            expected,
            model,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexedModel {
    pub(crate) request_digest: Hex32,
    pub(crate) agent: AgentId,
    pub(crate) model: Option<ModelName>,
}

#[derive(Debug, Default)]
pub(crate) struct DurableModelIndex {
    changes: BTreeMap<Hex32, IndexedModel>,
    /// Requests answered because the override already held, which append no record. They live
    /// for this run only, so a retry after another change to the agent still gets its first answer.
    held: BTreeMap<Hex32, IndexedModel>,
}

impl DurableModelIndex {
    pub(crate) fn replay(reader: &Reader) -> Result<Self> {
        let mut index = Self::default();
        for item in reader
            .iter_from(Lsn::ZERO)
            .context("reading Floor model index")?
        {
            let (_, entry) = item.context("reading Floor model journal entry")?;
            index.apply(&entry.record)?;
        }
        Ok(index)
    }

    pub(crate) fn apply(&mut self, record: &Record) -> Result<()> {
        let Record::AgentModelChanged {
            agent,
            model,
            request: Some(meta),
            ..
        } = record
        else {
            return Ok(());
        };
        if self.changes.contains_key(&meta.request_id_digest) {
            bail!("duplicate Floor model request in journal");
        }
        self.changes.insert(
            meta.request_id_digest,
            IndexedModel {
                request_digest: meta.request_digest,
                agent: *agent,
                model: model.clone(),
            },
        );
        Ok(())
    }

    pub(crate) fn remember_held(&mut self, request_id_digest: Hex32, held: IndexedModel) {
        self.held.insert(request_id_digest, held);
    }

    pub(crate) fn get(&self, request_id_digest: Hex32) -> Option<&IndexedModel> {
        self.changes
            .get(&request_id_digest)
            .or_else(|| self.held.get(&request_id_digest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(expected: Option<&str>, model: Option<&str>) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "request_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "agent": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "expected": expected, "model": model,
        }))
        .unwrap()
    }

    #[test]
    fn strict_fields_and_shape_checked_names() {
        let parsed = ModelRequest::parse(&body(Some("sonnet"), None)).unwrap();
        assert_eq!(
            parsed.expected.as_ref().map(ModelName::as_str),
            Some("sonnet")
        );
        assert!(parsed.model.is_none());
        assert_ne!(
            parsed.request_digest,
            ModelRequest::parse(&body(None, None))
                .unwrap()
                .request_digest,
            "the expectation is inside the digest"
        );
        for bad in ["-flag", "two words", "", &"m".repeat(65)] {
            assert!(
                ModelRequest::parse(&body(None, Some(bad))).is_err(),
                "{bad:?}"
            );
            assert!(
                ModelRequest::parse(&body(Some(bad), None)).is_err(),
                "{bad:?}"
            );
        }
        for bytes in [
            br#"{"request_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","model":"a"}"#.to_vec(),
            br#"{"request_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","expected":null,"model":"a","token":"x"}"#.to_vec(),
        ] {
            assert!(ModelRequest::parse(&bytes).is_err());
        }
    }
}
