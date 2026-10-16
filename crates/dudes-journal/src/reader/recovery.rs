//! Recovery classification over original top-level JSON, without normalizing duplicate members.

use dudes_proto::RecordKind;
use serde::de::IgnoredAny;

use crate::frame::Header;

pub(super) fn is_kind(kind: RecordKind) -> bool {
    matches!(
        kind,
        RecordKind::RecoveryRequired
            | RecordKind::RecoveryAuthorized
            | RecordKind::RecoveryInvalidated
    )
}

pub(super) fn payload(header: Header, payload: &[u8]) -> bool {
    RecordKind::from_tag(header.kind).is_some_and(is_kind) || discriminator(payload)
}

fn name(value: &str) -> bool {
    matches!(
        value,
        "recovery_required" | "recovery_authorized" | "recovery_invalidated"
    )
}

fn whitespace(bytes: &[u8]) -> &[u8] {
    let at = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        .unwrap_or(bytes.len());
    &bytes[at..]
}

fn discriminator(payload: &[u8]) -> bool {
    let Some(mut rest) = whitespace(payload).strip_prefix(b"{") else {
        return false;
    };
    loop {
        rest = whitespace(rest);
        let mut key = serde_json::Deserializer::from_slice(rest).into_iter::<String>();
        let Some(Ok(key_value)) = key.next() else {
            return false;
        };
        let Some(value) = whitespace(&rest[key.byte_offset()..]).strip_prefix(b":") else {
            return false;
        };
        let mut parsed = serde_json::Deserializer::from_slice(value).into_iter::<IgnoredAny>();
        if !matches!(parsed.next(), Some(Ok(_))) {
            return false;
        }
        let end = parsed.byte_offset();
        if key_value == "t"
            && serde_json::from_slice::<String>(&value[..end]).is_ok_and(|tag| name(&tag))
        {
            return true;
        }
        let Some(next) = whitespace(&value[end..]).strip_prefix(b",") else {
            return false;
        };
        rest = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_top_level_recovery_discriminators_survive_conflicting_members() {
        for name in [
            "recovery_required",
            "recovery_authorized",
            "recovery_invalidated",
        ] {
            for conflict in ["null", "1e9999", "{}", "\"other\"", &format!("\"{name}\"")] {
                for body in [
                    format!("{{\"t\":{conflict},\"t\":\"{name}\"}}"),
                    format!("{{\"t\":\"{name}\",\"t\":{conflict}}}"),
                ] {
                    assert!(discriminator(body.as_bytes()), "{body}");
                }
            }
            assert!(discriminator(
                format!("{{\"\\u0074\":\"{name}\" trailing").as_bytes()
            ));
            assert!(discriminator(
                format!("{{\"t\":\"{name}\"}} trailing").as_bytes()
            ));
        }
        assert!(discriminator(br#"{"t":"recovery_\u0072equired"}"#));
    }

    #[test]
    fn unknown_compatible_payloads_without_a_decoded_recovery_tag_keep_their_meaning() {
        for body in [
            br#"{"nested":{"t":"recovery_required"}}"#.as_slice(),
            br#"{"text":"recovery_authorized","t":"other"}"#,
            br#"{"broken":?,"t":"recovery_invalidated"}"#,
            b"arbitrary non-JSON compatible payload",
            br#"{"t":1e9999,"nested":"recovery_required"}"#,
        ] {
            assert!(!discriminator(body), "{body:?}");
        }
    }
}
