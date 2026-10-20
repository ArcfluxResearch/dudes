//! The Floor Settings page's edit of the operator file.
//!
//! The page can change only the keys in [`EDITABLE`]: the `[floor.*]` switches, the two budget
//! numbers and the engines. It never writes a provider command, an MCP server, a secret or a
//! project's check. The edit is the same `dudes_config` edit the CLI runs. A set is idempotent,
//! so the route keeps no request identity.

use dudes_config::config::{Change, Key};
use serde::Deserialize;

/// Most bytes one settings request body may carry.
pub const MAX_SETTINGS_BODY_BYTES: usize = 256;

/// The closed list of keys the page may write. Anything else is refused as `key_not_editable`.
pub const EDITABLE: [Key; 15] = [
    Key::Answer,
    Key::Hire,
    Key::Control,
    Key::Recovery,
    Key::Goal,
    Key::Renew,
    Key::Retire,
    Key::Board,
    Key::Terminal,
    Key::Git,
    Key::Diff,
    Key::Project,
    Key::FloorUsdPerDay,
    Key::PerTurnUsd,
    Key::PerTurnTokens,
];

/// Engines on or off. Kept apart from [`EDITABLE`] only because they are tables, not switches.
const ENGINES: [Key; 2] = [Key::Codex, Key::Pi];

/// One settings edit: a key from the closed list and a value that key takes.
#[derive(Clone, Debug)]
pub struct SettingsRequest {
    pub(crate) change: Change,
}

/// Why an edit was refused. Nothing was written in any case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsRefusal {
    /// The session expired or was revoked.
    Forbidden,
    /// The key is not on the closed list.
    KeyNotEditable,
    /// The key does not take that value.
    BadValue,
    /// The daemon has no operator file to edit.
    NoFile,
    /// The edit would leave a file the daemon refuses at start, for this reason.
    StartRule(String),
    /// The edit would take a per-turn reserve to zero, which can block the next start. The
    /// command line can still do it, with a warning.
    ZeroesReserve,
    /// The file could not be read, parsed or written.
    FileError,
    /// The daemon is ending.
    Unavailable,
}

impl SettingsRefusal {
    /// The fixed code the page shows.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Forbidden => "forbidden",
            Self::KeyNotEditable => "key_not_editable",
            Self::BadValue => "bad_value",
            Self::NoFile => "no_config_file",
            Self::StartRule(_) => "start_rule",
            Self::ZeroesReserve => "zeroes_reserve",
            Self::FileError => "file_error",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Why a saved edit is not running yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotLive {
    /// The file has drifted from the running projects, by a hand edit or a pending project
    /// change, so the daemon declined to adopt it. A restart adopts it.
    Drifted,
    /// The daemon could not read or adopt the file just now.
    Unread,
}

impl NotLive {
    /// The fixed code the page shows.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Drifted => "enrollment_drift",
            Self::Unread => "reload_failed",
        }
    }
}

/// What a saved edit did to the running daemon.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsSaved {
    /// Whether the file changed.
    pub wrote: bool,
    /// The switches the running daemon now runs, by key.
    pub applied: Vec<String>,
    /// The settings that take a restart.
    pub restart: Vec<String>,
    /// Set when the file was saved but the running daemon did not adopt it.
    pub not_live: Option<NotLive>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    key: String,
    value: String,
}

impl SettingsRequest {
    /// Decode one body. `Err(None)` is a body that is not the two strings; `Err(Some(_))` is a
    /// well-formed request the daemon refuses.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, Option<SettingsRefusal>> {
        if bytes.len() > MAX_SETTINGS_BODY_BYTES {
            return Err(None);
        }
        let body: Body = serde_json::from_slice(bytes).map_err(|_| None)?;
        let key = Key::named(&body.key)
            .filter(|key| EDITABLE.contains(key) || ENGINES.contains(key))
            .ok_or(Some(SettingsRefusal::KeyNotEditable))?;
        let change = Change::set(key, &body.value).map_err(|_| Some(SettingsRefusal::BadValue))?;
        Ok(Self { change })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(key: &str, value: &str) -> Result<String, Option<SettingsRefusal>> {
        let body = serde_json::to_vec(&serde_json::json!({"key": key, "value": value})).unwrap();
        SettingsRequest::parse(&body).map(|request| request.change.to_string())
    }

    /// The page can write a switch, a budget number and an engine, and nothing else the CLI can.
    #[test]
    fn a_key_outside_the_closed_list_is_refused() {
        assert_eq!(
            parse("floor.answer", "on"),
            Ok("floor.answer on".to_owned())
        );
        assert_eq!(
            parse("budget.floor_usd_per_day", "5"),
            Ok("budget.floor_usd_per_day 5".to_owned())
        );
        assert_eq!(
            parse("providers.pi", "on"),
            Ok("providers.pi on".to_owned())
        );
        for key in [
            "providers.codex.streaming",
            "mcp.servers",
            "providers.pi.command",
            "",
        ] {
            assert_eq!(
                parse(key, "on"),
                Err(Some(SettingsRefusal::KeyNotEditable)),
                "{key}"
            );
        }
        assert_eq!(
            parse("floor.answer", "yes"),
            Err(Some(SettingsRefusal::BadValue))
        );
        assert_eq!(
            parse("budget.floor_usd_per_day", "-1"),
            Err(Some(SettingsRefusal::BadValue))
        );
    }

    /// Every key `dudes config` knows is either on the page's list or deliberately kept off it.
    #[test]
    fn the_list_names_every_key_but_streaming() {
        for key in Key::ALL {
            let listed = EDITABLE.contains(&key) || ENGINES.contains(&key);
            assert_eq!(listed, key != Key::CodexStreaming, "{key}");
        }
    }

    #[test]
    fn a_body_with_another_member_is_not_a_request() {
        let body = br#"{"key":"floor.answer","value":"on","command":"sh"}"#;
        assert!(matches!(SettingsRequest::parse(body), Err(None)));
    }
}
