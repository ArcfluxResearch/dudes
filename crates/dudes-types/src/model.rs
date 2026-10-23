//! The model an agent is hired to run: a name the engine resolves, checked here only for shape.

use alloc::borrow::Cow;
use alloc::string::String;
use core::fmt;
use core::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

/// The text form: ASCII letters, digits and `.`, `_`, `-`, `:`, `/`, `@`, `[`, `]`, not starting
/// with `-`. The brackets let `sonnet[1m]` through.
pub const MODEL_NAME_PATTERN: &str = r"^[A-Za-z0-9._:/@\[\]][A-Za-z0-9._:/@\[\]-]*$";

/// Most bytes in a model name.
pub const MAX_MODEL_NAME_BYTES: usize = 64;

/// A name outside [`MODEL_NAME_PATTERN`] or longer than [`MAX_MODEL_NAME_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error(
    "invalid model name: expected 1 to 64 ASCII letters, digits and . _ - : / @ [ ], not starting with -"
)]
pub struct ParseModelNameError;

/// One model name. The daemon checks its shape and nothing else: an engine that does not know the
/// name fails the agent's first turn with its own error. The shape keeps the name from being read
/// as an option when it is one argv element after the engine's model flag.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModelName(String);

impl ModelName {
    /// Parse exactly the text [`MODEL_NAME_PATTERN`] matches, at most [`MAX_MODEL_NAME_BYTES`] long.
    ///
    /// # Errors
    ///
    /// Returns [`ParseModelNameError`] for any other text, an empty one included.
    pub fn parse(text: &str) -> Result<Self, ParseModelNameError> {
        let bytes = text.as_bytes();
        let body = |b: &u8| b.is_ascii_alphanumeric() || b"._:/@[]".contains(b);
        let first_ok = bytes.first().is_some_and(body);
        let rest_ok = bytes.iter().all(|b| body(b) || *b == b'-');
        if first_ok && rest_ok && bytes.len() <= MAX_MODEL_NAME_BYTES {
            Ok(Self(String::from(text)))
        } else {
            Err(ParseModelNameError)
        }
    }

    /// The name as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ModelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ModelName({})", self.0)
    }
}

impl FromStr for ModelName {
    type Err = ParseModelNameError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl Serialize for ModelName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ModelName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = ModelName;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a model name")
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<ModelName, E> {
                ModelName::parse(text).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

impl JsonSchema for ModelName {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ModelName")
    }

    fn schema_id() -> Cow<'static, str> {
        Cow::Borrowed("dudes_types::ModelName")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "ModelName: the model an agent is hired to run.",
            "pattern": MODEL_NAME_PATTERN,
            "minLength": 1,
            "maxLength": MAX_MODEL_NAME_BYTES,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_ascii_words_that_never_start_with_a_dash() {
        for good in [
            "opus",
            "gpt-5-codex",
            "openai/gpt-4o:high",
            "a@b.c_d",
            "0",
            "sonnet[1m]",
        ] {
            assert!(ModelName::parse(good).is_ok(), "{good}");
        }
        assert!(ModelName::parse(&"a".repeat(64)).is_ok());
        for bad in ["", "-m", "--model", "a b", "a\n", "é", "a;b", "a=b"] {
            assert_eq!(ModelName::parse(bad), Err(ParseModelNameError), "{bad:?}");
        }
        assert_eq!(ModelName::parse(&"a".repeat(65)), Err(ParseModelNameError));
    }

    #[test]
    fn a_name_round_trips_and_a_bad_one_is_refused_on_decode() {
        let name = ModelName::parse("sonnet").unwrap();
        assert_eq!(serde_json::to_string(&name).unwrap(), "\"sonnet\"");
        assert_eq!(
            serde_json::from_str::<ModelName>("\"sonnet\"").unwrap(),
            name
        );
        assert!(serde_json::from_str::<ModelName>("\"-x\"").is_err());
    }
}
