//! The signature a Floor page puts on every request, and the check of it.
//!
//! The page signs eight length-prefixed fields with a P-256 key it cannot export: a fixed label,
//! its device id, the method, the exact request target, its origin, the SHA-256 of the body, the
//! time in milliseconds and a random request id. This module parses what a request presents and
//! verifies it under the device's public key. It decides nothing about time or replays: those need
//! the clock and the device's memory, which the session registry holds.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dudes_proto::record::{DeviceId, Hex32};
use p256::ecdsa::signature::Verifier as _;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::pkcs8::{DecodePublicKey as _, EncodePublicKey as _};
use sha2::{Digest as _, Sha256};

/// The label that binds a signature to Floor requests, so it cannot stand for anything else.
const LABEL: &[u8] = b"dudes-floor-request-v1";
/// The longest time a request may name, in digits.
const MAX_TIME_DIGITS: usize = 16;
/// The characters in a request id: 16 bytes of unpadded base64url.
const REQUEST_ID_LEN: usize = 22;
/// The characters in a signature: 64 bytes of unpadded base64url.
const SIGNATURE_LEN: usize = 86;
/// The longest public key a page may send, as SPKI DER. A P-256 key is 91 bytes.
const MAX_KEY_CHARS: usize = 160;

/// A device's public key, the only thing that can show a request came from its browser.
#[derive(Clone)]
pub(crate) struct DeviceKey(VerifyingKey);

impl DeviceKey {
    /// Read a P-256 public key from SPKI DER in unpadded base64url. The key must be the
    /// canonical encoding, so one key has one id. The device id is the SHA-256 of the bytes read.
    pub(crate) fn parse(text: &str) -> Option<(Self, Hex32)> {
        if text.is_empty() || text.len() > MAX_KEY_CHARS {
            return None;
        }
        let der = URL_SAFE_NO_PAD.decode(text).ok()?;
        let key = VerifyingKey::from_public_key_der(&der).ok()?;
        if key.to_public_key_der().ok()?.as_bytes() != der.as_slice() {
            return None;
        }
        Some((Self(key), Hex32::new(Sha256::digest(&der).into())))
    }

    /// The key as it is journaled: SPKI DER in unpadded base64url.
    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        let der = self
            .0
            .to_public_key_der()
            .expect("a verifying key encodes as SPKI");
        URL_SAFE_NO_PAD.encode(der.as_bytes())
    }

    /// Whether `presented` is this key's signature over `subject`, made for `device`. A signature
    /// is accepted with either value of `s`, because the browser's does not normalize it.
    pub(crate) fn verifies(
        &self,
        device: Hex32,
        subject: &Subject<'_>,
        presented: &Presented,
    ) -> bool {
        let message = message(device, subject, presented);
        self.0.verify(&message, &presented.signature).is_ok()
    }
}

/// The part of a request the daemon read for itself.
pub(crate) struct Subject<'a> {
    pub(crate) method: &'a str,
    /// The request target exactly as the request line carried it.
    pub(crate) target: &'a str,
    pub(crate) body: &'a [u8],
}

/// What a request presents: the origin the page claims, when it signed, a one-time id and the
/// signature. Parsed for form only; nothing here has been checked against a device yet.
#[derive(Clone)]
pub(crate) struct Presented {
    pub(crate) origin: String,
    pub(crate) millis: i64,
    time: String,
    pub(crate) request: [u8; 16],
    request_text: String,
    signature: Signature,
}

impl Presented {
    /// Read the four values a request carries. None when any is missing its form.
    pub(crate) fn parse(origin: &str, time: &str, request: &str, signature: &str) -> Option<Self> {
        if time.is_empty()
            || time.len() > MAX_TIME_DIGITS
            || !time.bytes().all(|byte| byte.is_ascii_digit())
            || (time.len() > 1 && time.starts_with('0'))
            || request.len() != REQUEST_ID_LEN
            || signature.len() != SIGNATURE_LEN
        {
            return None;
        }
        let request_bytes = URL_SAFE_NO_PAD.decode(request).ok()?;
        let signature_bytes = URL_SAFE_NO_PAD.decode(signature).ok()?;
        Some(Self {
            origin: origin.to_owned(),
            millis: time.parse().ok()?,
            time: time.to_owned(),
            request: request_bytes.try_into().ok()?,
            request_text: request.to_owned(),
            signature: Signature::from_slice(&signature_bytes).ok()?,
        })
    }
}

/// The bytes the page signed: each field behind its length as four big-endian bytes.
fn message(device: Hex32, subject: &Subject<'_>, presented: &Presented) -> Vec<u8> {
    let device = DeviceId::from(device).to_string();
    let body = Sha256::digest(subject.body);
    let fields: [&[u8]; 8] = [
        LABEL,
        device.as_bytes(),
        subject.method.as_bytes(),
        subject.target.as_bytes(),
        presented.origin.as_bytes(),
        &body,
        presented.time.as_bytes(),
        presented.request_text.as_bytes(),
    ];
    let mut message = Vec::with_capacity(fields.iter().map(|field| 4 + field.len()).sum());
    for field in fields {
        let length = u32::try_from(field.len()).expect("a request field is far under 4 GiB");
        message.extend_from_slice(&length.to_be_bytes());
        message.extend_from_slice(field);
    }
    message
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    const VECTOR: &str = include_str!("../../../../floor/tests/fixtures/signed-request-v1.json");

    struct Vector {
        spki: String,
        device: String,
        method: String,
        target: String,
        origin: String,
        body: String,
        time: String,
        request: String,
        message: String,
        signature: String,
    }

    fn vector() -> Vector {
        let json: Value = serde_json::from_str(VECTOR).expect("the vector is JSON");
        let text = |value: &Value| value.as_str().expect("a string").to_owned();
        let request = &json["request"];
        Vector {
            spki: text(&json["spki"]),
            device: text(&json["device"]),
            method: text(&request["method"]),
            target: text(&request["target"]),
            origin: text(&request["origin"]),
            body: text(&request["body"]),
            time: text(&request["time"]),
            request: text(&request["request_id"]),
            message: text(&json["message"]),
            signature: text(&json["signature"]),
        }
    }

    fn check(vector: &Vector) -> bool {
        let Some((key, device)) = DeviceKey::parse(&vector.spki) else {
            return false;
        };
        if DeviceId::from(device).to_string() != vector.device {
            return false;
        }
        let Some(presented) = Presented::parse(
            &vector.origin,
            &vector.time,
            &vector.request,
            &vector.signature,
        ) else {
            return false;
        };
        key.verifies(
            device,
            &Subject {
                method: &vector.method,
                target: &vector.target,
                body: vector.body.as_bytes(),
            },
            &presented,
        )
    }

    #[test]
    fn the_browser_made_vector_verifies_with_its_high_s_signature() {
        let vector = vector();
        let (_, device) = DeviceKey::parse(&vector.spki).expect("the key");
        let presented = Presented::parse(
            &vector.origin,
            &vector.time,
            &vector.request,
            &vector.signature,
        )
        .expect("the request values");
        assert_ne!(
            presented.signature.normalize_s(),
            presented.signature,
            "the vector's s is the high one"
        );
        let subject = Subject {
            method: &vector.method,
            target: &vector.target,
            body: vector.body.as_bytes(),
        };
        assert_eq!(
            URL_SAFE_NO_PAD.encode(message(device, &subject, &presented)),
            vector.message
        );
        assert!(check(&vector));
    }

    #[test]
    fn a_one_byte_change_to_any_field_fails() {
        let flip = |text: &str| {
            let mut bytes = text.as_bytes().to_vec();
            bytes[2] = if bytes[2] == b'x' { b'y' } else { b'x' };
            String::from_utf8(bytes).expect("ASCII")
        };
        type Change = fn(&mut Vector, String);
        let changes: [(&str, fn(&Vector) -> String, Change); 7] = [
            ("device", |v| v.device.clone(), |v, t| v.device = t),
            ("method", |v| v.method.clone(), |v, t| v.method = t),
            ("target", |v| v.target.clone(), |v, t| v.target = t),
            ("origin", |v| v.origin.clone(), |v, t| v.origin = t),
            ("body", |v| v.body.clone(), |v, t| v.body = t),
            ("time", |v| v.time.clone(), |v, t| v.time = t),
            ("request", |v| v.request.clone(), |v, t| v.request = t),
        ];
        for (name, read, write) in changes {
            let mut changed = vector();
            let text = read(&changed);
            write(&mut changed, flip(&text));
            assert!(!check(&changed), "a changed {name} verified");
        }
        let mut changed = vector();
        changed.signature = flip(&changed.signature);
        assert!(!check(&changed), "a changed signature verified");
    }

    #[test]
    fn a_request_with_malformed_values_presents_nothing() {
        let vector = vector();
        let parse = |time: &str, request: &str, signature: &str| {
            Presented::parse(&vector.origin, time, request, signature).is_some()
        };
        assert!(parse(&vector.time, &vector.request, &vector.signature));
        assert!(!parse("", &vector.request, &vector.signature));
        assert!(!parse("01761100000000", &vector.request, &vector.signature));
        assert!(!parse("-1761100000000", &vector.request, &vector.signature));
        assert!(!parse(
            &vector.time,
            &vector.request[1..],
            &vector.signature
        ));
        assert!(!parse(
            &vector.time,
            &vector.request,
            &vector.signature[1..]
        ));
        assert!(!parse(
            &vector.time,
            &format!("{}=", &vector.request[1..]),
            &vector.signature
        ));
    }

    #[test]
    fn only_a_canonical_p256_key_is_a_device_key() {
        let vector = vector();
        assert!(DeviceKey::parse(&vector.spki).is_some());
        assert!(DeviceKey::parse("").is_none());
        assert!(DeviceKey::parse(&vector.spki[1..]).is_none());
        assert!(DeviceKey::parse(&format!("{}AAAA", vector.spki)).is_none());
        assert!(DeviceKey::parse(&vector.device).is_none());
    }
}
