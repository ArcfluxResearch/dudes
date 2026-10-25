//! A bounded browser request to ask an engine for its model list again.

use anyhow::{Context as _, Result, anyhow, bail};
use dudes_proto::record::Hex32;
use dudes_types::Provider;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// Maximum encoded body: an attempt id and an engine name, with room for escaping.
pub const MAX_REFRESH_BODY_BYTES: usize = 256;

/// Canonical fields admitted before any listing child starts.
#[derive(Clone)]
pub struct RefreshRequest {
    pub(crate) request_id: Hex32,
    pub(crate) request_wire: String,
    pub(crate) request_digest: Hex32,
    pub(crate) provider: Provider,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshJson {
    request_id: String,
    provider: String,
}

impl RefreshRequest {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_REFRESH_BODY_BYTES {
            bail!("Floor model refresh request is too large");
        }
        let raw: RefreshJson =
            serde_json::from_slice(bytes).context("invalid Floor model refresh JSON")?;
        let request_id = crate::floor_actions::decode_attempt(&raw.request_id)
            .context("invalid Floor model refresh request id")?;
        let provider = Provider::ALL
            .into_iter()
            .find(|provider| provider.to_string() == raw.provider)
            .ok_or_else(|| anyhow!("unknown Floor model refresh engine"))?;
        let mut hasher = Sha256::new();
        hasher.update(b"dudes.models.refresh.v1\0");
        hasher.update(provider.to_string().as_bytes());
        Ok(Self {
            request_id,
            request_wire: raw.request_id,
            request_digest: Hex32::new(hasher.finalize().into()),
            provider,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    #[test]
    fn an_exact_body_names_one_engine_and_binds_it_into_the_digest() {
        let pi =
            RefreshRequest::parse(format!(r#"{{"request_id":"{ID}","provider":"pi"}}"#).as_bytes())
                .expect("a request");
        assert_eq!(pi.provider, Provider::Pi);
        assert_eq!(pi.request_wire, ID);
        let codex = RefreshRequest::parse(
            format!(r#"{{"request_id":"{ID}","provider":"codex"}}"#).as_bytes(),
        )
        .expect("a request");
        assert_ne!(pi.request_digest, codex.request_digest);
    }

    #[test]
    fn anything_else_is_refused() {
        for body in [
            format!(r#"{{"request_id":"{ID}","provider":"gemini"}}"#),
            format!(r#"{{"request_id":"{ID}"}}"#),
            format!(r#"{{"request_id":"{ID}","provider":"pi","agent":"x"}}"#),
            r#"{"request_id":"short","provider":"pi"}"#.to_owned(),
            "[]".to_owned(),
            format!(
                r#"{{"request_id":"{ID}","provider":"{}"}}"#,
                "p".repeat(300)
            ),
        ] {
            assert!(RefreshRequest::parse(body.as_bytes()).is_err(), "{body}");
        }
    }
}
