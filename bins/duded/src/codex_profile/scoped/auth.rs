use std::fmt;
use std::fs::{File, Metadata};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Deserialize;
use serde::de::{self, MapAccess, Visitor};

use super::{inspection, refusal};

const AUTH_LIMIT: u64 = 16 * 1024;

pub(super) struct AuthBinding {
    path: PathBuf,
    file: File,
    metadata: Metadata,
    projection: Projection,
}

impl AuthBinding {
    pub(super) fn capture(home: &Path, deadline: Instant) -> io::Result<Self> {
        let path = home.join("auth.json");
        let file = inspection::open_file(&path)
            .map_err(|_| refusal("scoped API-key auth file is unavailable"))?;
        let metadata = file.metadata()?;
        let projection = project(&file, deadline)?;
        let binding = Self {
            path,
            file,
            metadata,
            projection,
        };
        binding.check(deadline)?;
        Ok(binding)
    }

    pub(super) fn check(&self, deadline: Instant) -> io::Result<()> {
        let current = inspection::open_file(&self.path)
            .map_err(|_| refusal("scoped API-key auth file changed"))?;
        let metadata = current.metadata()?;
        if !same_metadata(&metadata, &self.metadata)?
            || !same_metadata(&self.file.metadata()?, &self.metadata)?
            || project(&current, deadline)? != self.projection
            || !same_metadata(&current.metadata()?, &self.metadata)?
            || !same_metadata(&std::fs::symlink_metadata(&self.path)?, &self.metadata)?
        {
            return Err(refusal("scoped API-key auth file changed"));
        }
        Ok(())
    }
}

impl fmt::Debug for AuthBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthBinding")
            .field("projection", &self.projection)
            .finish_non_exhaustive()
    }
}

fn same_metadata(left: &Metadata, right: &Metadata) -> io::Result<bool> {
    Ok(inspection::same(left, right)
        && left.len() == right.len()
        && left.modified()? == right.modified()?
        && inspection::changed(left) == inspection::changed(right))
}

fn project(file: &File, deadline: Instant) -> io::Result<Projection> {
    let metadata = file.metadata()?;
    if Instant::now() >= deadline
        || metadata.len() == 0
        || metadata.len() > AUTH_LIMIT
        || !owned(&metadata)
    {
        return Err(refusal("scoped auth metadata is unqualified"));
    }
    let mut json = serde_json::Deserializer::from_reader(file.take(AUTH_LIMIT + 1));
    let projection = Projection::deserialize(&mut json)
        .map_err(|_| refusal("scoped auth metadata is unqualified"))?;
    json.end()
        .map_err(|_| refusal("scoped auth metadata is unqualified"))?;
    if Instant::now() >= deadline {
        return Err(refusal("scoped auth metadata inspection expired"));
    }
    Ok(projection)
}

#[cfg(unix)]
fn owned(metadata: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    metadata.uid() == nix::unistd::geteuid().as_raw() && metadata.mode() & 0o022 == 0
}

#[cfg(not(unix))]
fn owned(_metadata: &Metadata) -> bool {
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Projection {
    mode: ApiKeyMode,
    key: PresentKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ApiKeyMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PresentKey;

impl<'de> Deserialize<'de> for Projection {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AuthVisitor;
        impl<'de> Visitor<'de> for AuthVisitor {
            type Value = Projection;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("qualified API-key auth metadata")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut mode = None;
                let mut key = None;
                let mut fields = std::collections::BTreeSet::new();
                while let Some(field) = map.next_key::<String>()? {
                    if !fields.insert(field.clone()) {
                        return Err(de::Error::custom("unqualified auth metadata"));
                    }
                    match field.as_str() {
                        "auth_mode" => mode = Some(map.next_value()?),
                        "OPENAI_API_KEY" => key = Some(map.next_value()?),
                        "tokens"
                        | "agent_identity"
                        | "personal_access_token"
                        | "bedrock_api_key"
                        | "bedrock_access_keys" => {
                            if map.next_value::<Option<de::IgnoredAny>>()?.is_some() {
                                return Err(de::Error::custom("unqualified auth metadata"));
                            }
                        }
                        "last_refresh" => {
                            map.next_value::<Option<LastRefresh>>()?;
                        }
                        _ => {
                            map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(Projection {
                    mode: mode.ok_or_else(|| de::Error::custom("unqualified auth metadata"))?,
                    key: key.ok_or_else(|| de::Error::custom("unqualified auth metadata"))?,
                })
            }
        }
        deserializer.deserialize_map(AuthVisitor)
    }
}

impl<'de> Deserialize<'de> for ApiKeyMode {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ModeVisitor;
        impl Visitor<'_> for ModeVisitor {
            type Value = ApiKeyMode;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("API-key auth mode")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value == "apikey" {
                    Ok(ApiKeyMode)
                } else {
                    Err(de::Error::custom("unqualified auth metadata"))
                }
            }
        }
        deserializer.deserialize_str(ModeVisitor)
    }
}

impl<'de> Deserialize<'de> for PresentKey {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyVisitor;
        impl Visitor<'_> for KeyVisitor {
            type Value = PresentKey;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a present API key")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value.trim().is_empty() {
                    Err(de::Error::custom("unqualified auth metadata"))
                } else {
                    Ok(PresentKey)
                }
            }
        }
        deserializer.deserialize_str(KeyVisitor)
    }
}

struct LastRefresh;

impl<'de> Deserialize<'de> for LastRefresh {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TimestampVisitor;
        impl Visitor<'_> for TimestampVisitor {
            type Value = LastRefresh;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an auth refresh timestamp")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                chrono::DateTime::parse_from_rfc3339(value)
                    .map(|_| LastRefresh)
                    .map_err(|_| de::Error::custom("unqualified auth metadata"))
            }
        }
        deserializer.deserialize_str(TimestampVisitor)
    }
}
