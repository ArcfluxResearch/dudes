//! `dudes config`: show and edit the settings the daemon reads from the operator's file only when
//! it starts.
//!
//! An edit keeps the file's other settings and comments where `toml_edit` can. It writes line
//! endings as LF, can respace a header it rewrites, and drops the comments of a table that `unset`
//! empties. It is written as `dudes project` writes, to a temporary file beside the original that
//! is renamed over it.
//!
//! An edit that breaks the file's own start rules, [`RULES`], is refused before anything is
//! written, and the refusal names the key that is missing. The daemon also refuses to start while
//! an agent it has hired, or that agent's goal, has a ceiling in a unit whose per-turn reserve is
//! zero. Only its journal knows that, so an edit that takes a reserve to zero is written and
//! [`Edited::warnings`] says what it may cost.
//!
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use dudes_proto::Spend;
use dudes_types::{Provider, Usd};
use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

use crate::workspace::{self, NewWorkspace, WorkspaceError};

/// One setting `dudes config` reads and writes, spelled as its TOML dotted key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// `[providers.codex]`: the Codex adapter, on while the table exists.
    Codex,
    /// `[providers.codex] streaming`: opt in to the separate streaming sign-in home.
    CodexStreaming,
    /// `[providers.pi]`: the Pi adapter, on while the table exists.
    Pi,
    /// `[floor.answer] enabled`: the Floor answers a waiting agent's question.
    Answer,
    /// `[floor.hire] enabled`: the Floor hires an agent.
    Hire,
    /// `[floor.control] enabled`: the Floor pauses, resumes and renames an agent.
    Control,
    /// `[floor.recovery] enabled`: the Floor recovers interrupted managed work.
    Recovery,
    /// `[floor.goal] enabled`: the Floor changes an agent's goal.
    Goal,
    /// `[floor.renew] enabled`: the Floor gives an agent whose goal has nothing left a new goal
    /// with its workspace's allowance.
    Renew,
    /// `[floor.retire] enabled`: the Floor retires an agent.
    Retire,
    /// `[floor.board] enabled`: the Floor makes tickets and gives them to agents, which then run
    /// and spend under your account.
    Board,
    /// `[floor.terminal] enabled`: the Floor attaches a terminal.
    Terminal,
    /// `[floor.git] enabled`: the Floor reads an agent's checkout for its Git tab.
    Git,
    /// `[floor.diff] enabled`: with `floor.git` also on, the Floor reads a ticket's changed files
    /// for its Changes view.
    Diff,
    /// `[budget] floor_usd_per_day`: the floor-wide daily dollar ceiling; none when absent.
    FloorUsdPerDay,
    /// `[budget.per_turn_floor] usd`: the dollars reserved for each turn whose cost is unseen.
    PerTurnUsd,
    /// `[budget.per_turn_floor] tokens`: the tokens reserved for each turn whose cost is unseen.
    PerTurnTokens,
}

/// Where a key lives in the file, and what it holds there.
#[derive(Clone, Copy)]
enum Place {
    /// A table whose presence is the setting.
    Table(&'static [&'static str]),
    Switch(&'static [&'static str], &'static str),
    /// Dollars in one entry of a table.
    Dollars(&'static [&'static str], &'static str),
    /// A token count in one entry of a table.
    Tokens(&'static [&'static str], &'static str),
}

impl Place {
    /// The tables that lead to the key.
    const fn path(self) -> &'static [&'static str] {
        match self {
            Self::Table(path)
            | Self::Switch(path, _)
            | Self::Dollars(path, _)
            | Self::Tokens(path, _) => path,
        }
    }

    /// The key's entry in the last table; `None` for a table whose presence is the setting.
    const fn entry(self) -> Option<&'static str> {
        match self {
            Self::Table(_) => None,
            Self::Switch(_, entry) | Self::Dollars(_, entry) | Self::Tokens(_, entry) => {
                Some(entry)
            }
        }
    }
}

/// The file's own start rules: when the first key is on or set, the daemon starts only while the
/// second is on or above zero. Goal change and retirement give the first two rows, renewal's
/// need for goal change and Hire the next two, the board's need for goal change and pause and
/// resume the two after, and the boot check that a floor ceiling has a dollar reserve the last.
const RULES: [(Key, Key); 7] = [
    (Key::Goal, Key::Control),
    (Key::Retire, Key::Control),
    (Key::Renew, Key::Goal),
    (Key::Renew, Key::Hire),
    (Key::Board, Key::Goal),
    (Key::Board, Key::Control),
    (Key::FloorUsdPerDay, Key::PerTurnUsd),
];

impl Key {
    /// Every key, in the order `dudes config` shows them.
    pub const ALL: [Self; 17] = [
        Self::Codex,
        Self::CodexStreaming,
        Self::Pi,
        Self::Answer,
        Self::Hire,
        Self::Control,
        Self::Recovery,
        Self::Goal,
        Self::Renew,
        Self::Retire,
        Self::Board,
        Self::Terminal,
        Self::Git,
        Self::Diff,
        Self::FloorUsdPerDay,
        Self::PerTurnUsd,
        Self::PerTurnTokens,
    ];

    /// The key as `dudes config` takes it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Codex => "providers.codex",
            Self::CodexStreaming => "providers.codex.streaming",
            Self::Pi => "providers.pi",
            Self::Answer => "floor.answer",
            Self::Hire => "floor.hire",
            Self::Control => "floor.control",
            Self::Recovery => "floor.recovery",
            Self::Goal => "floor.goal",
            Self::Renew => "floor.renew",
            Self::Retire => "floor.retire",
            Self::Board => "floor.board",
            Self::Terminal => "floor.terminal",
            Self::Git => "floor.git",
            Self::Diff => "floor.diff",
            Self::FloorUsdPerDay => "budget.floor_usd_per_day",
            Self::PerTurnUsd => "budget.per_turn_floor.usd",
            Self::PerTurnTokens => "budget.per_turn_floor.tokens",
        }
    }

    /// The provider whose adapter this key's table turns on.
    #[must_use]
    pub const fn adapter(self) -> Option<Provider> {
        match self {
            Self::Codex => Some(Provider::Codex),
            Self::Pi => Some(Provider::Pi),
            _ => None,
        }
    }

    const fn place(self) -> Place {
        match self {
            Self::Codex => Place::Table(&["providers", "codex"]),
            Self::CodexStreaming => Place::Switch(&["providers", "codex"], "streaming"),
            Self::Pi => Place::Table(&["providers", "pi"]),
            Self::Answer => Place::Switch(&["floor", "answer"], "enabled"),
            Self::Hire => Place::Switch(&["floor", "hire"], "enabled"),
            Self::Control => Place::Switch(&["floor", "control"], "enabled"),
            Self::Recovery => Place::Switch(&["floor", "recovery"], "enabled"),
            Self::Goal => Place::Switch(&["floor", "goal"], "enabled"),
            Self::Renew => Place::Switch(&["floor", "renew"], "enabled"),
            Self::Retire => Place::Switch(&["floor", "retire"], "enabled"),
            Self::Board => Place::Switch(&["floor", "board"], "enabled"),
            Self::Terminal => Place::Switch(&["floor", "terminal"], "enabled"),
            Self::Git => Place::Switch(&["floor", "git"], "enabled"),
            Self::Diff => Place::Switch(&["floor", "diff"], "enabled"),
            Self::FloorUsdPerDay => Place::Dollars(&["budget"], "floor_usd_per_day"),
            Self::PerTurnUsd => Place::Dollars(&["budget", "per_turn_floor"], "usd"),
            Self::PerTurnTokens => Place::Tokens(&["budget", "per_turn_floor"], "tokens"),
        }
    }

    /// The key `text` names.
    #[must_use]
    pub fn named(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.name() == text)
    }

    /// What the daemon reads when the key is absent: off, no floor ceiling, or the default
    /// per-turn reserve.
    fn absent(self) -> Held {
        let reserve = Spend::DEFAULT_PER_TURN_FLOOR;
        match (self, self.place()) {
            (Self::PerTurnUsd, _) => {
                let text = reserve.usd.to_decimal();
                let text = text.trim_end_matches('0').trim_end_matches('.').to_owned();
                Held::Dollars(Some((reserve.usd, text)))
            }
            (Self::PerTurnTokens, _) => Held::Tokens(Some(reserve.tokens.count())),
            (_, Place::Table(_) | Place::Switch(..)) => Held::Switch(false),
            (_, Place::Dollars(..)) => Held::Dollars(None),
            (_, Place::Tokens(..)) => Held::Tokens(None),
        }
    }
}

/// Every key's name, comma-separated.
#[must_use]
pub fn key_list() -> String {
    Key::ALL.map(Key::name).join(", ")
}

impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// One edit: a key and the TOML value text it becomes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    key: Key,
    /// `None` removes the key, so the daemon's default applies. For an adapter's table any `Some`
    /// makes the table exist.
    value: Option<String>,
}

impl Change {
    /// `key` set to `text`.
    ///
    /// # Errors
    ///
    /// What the key takes, when `text` is not one.
    pub fn set(key: Key, text: &str) -> Result<Self, &'static str> {
        let value = match (key.place(), text) {
            (Place::Table(_), "on") => Some(String::new()),
            (Place::Table(_), "off") => None,
            (Place::Switch(..), "on") => Some("true".to_owned()),
            (Place::Switch(..), "off") => Some("false".to_owned()),
            (Place::Table(_) | Place::Switch(..), _) => return Err("on or off"),
            (Place::Dollars(..), _) => {
                text.parse::<Value>()
                    .ok()
                    .filter(|value| dollars(value).is_some() && repr(value) == text)
                    .ok_or("dollars of zero or more, with at most six decimal places")?;
                Some(text.to_owned())
            }
            (Place::Tokens(..), _) => {
                text.parse::<i64>()
                    .ok()
                    .filter(|count| *count >= 0 && count.to_string() == text)
                    .ok_or("a whole number of tokens")?;
                Some(text.to_owned())
            }
        };
        Ok(Self { key, value })
    }

    /// `key` removed from the file.
    #[must_use]
    pub const fn unset(key: Key) -> Self {
        Self { key, value: None }
    }
}

/// The key and the value it becomes, as `dudes config set` takes them.
impl fmt::Display for Change {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match (self.key.place(), self.value.as_deref()) {
            (Place::Table(_), Some(_)) | (Place::Switch(..), Some("true")) => "on",
            (Place::Table(_) | Place::Switch(..), _) => "off",
            (_, Some(text)) => text,
            (_, None) => "unset",
        };
        write!(formatter, "{} {value}", self.key)
    }
}

/// Why the daemon would refuse the file at start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// `key` is on or set, and the daemon starts with it only while `needs` is on or above zero.
    Needs {
        /// The key that asks.
        key: Key,
        /// The key it needs.
        needs: Key,
    },
    /// `key`, or a table on its way, holds a value the daemon does not read as one.
    Unreadable(Key),
}

impl fmt::Display for Problem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let wanted = |key: Key| match key.place() {
            Place::Table(_) | Place::Switch(..) => "on",
            Place::Dollars(..) | Place::Tokens(..) => "above zero",
        };
        match *self {
            Self::Needs { key, needs } => match key.place() {
                Place::Table(_) | Place::Switch(..) => {
                    write!(formatter, "{key} on needs {needs} {}", wanted(needs))
                }
                Place::Dollars(..) | Place::Tokens(..) => {
                    write!(formatter, "{key} needs {needs} {}", wanted(needs))
                }
            },
            Self::Unreadable(key) => {
                write!(formatter, "{key} holds a value the daemon does not read")
            }
        }
    }
}

/// Why `dudes config` did nothing. The file is unchanged in every case.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file is not TOML.
    #[error("{0} is not a TOML document")]
    NotToml(PathBuf),
    /// The edit would leave a file the daemon refuses at start.
    #[error(
        "the daemon would refuse this configuration when it starts, so nothing was written: {}",
        list(.0)
    )]
    Refused(Vec<Problem>),
    /// Turning an adapter off would delete the other settings its table holds, such as a pinned
    /// `command`.
    #[error(
        "{key} holds other settings ({held}) that turning it off would delete; remove them from \
         the file by hand first if you mean to"
    )]
    Holds {
        /// The adapter's key.
        key: Key,
        /// The table's other entries, each in backticks.
        held: String,
    },
    /// Reading or writing the file failed.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// What failed.
        source: io::Error,
    },
}

fn list(problems: &[Problem]) -> String {
    problems
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// The operator file as the daemon will read it at its next start.
#[derive(Debug)]
pub struct Settings {
    document: DocumentMut,
    exists: bool,
}

impl Settings {
    /// Read the file; an absent one holds every default.
    ///
    /// # Errors
    ///
    /// A file that cannot be read or is not TOML.
    pub fn read(config: &Path) -> Result<Self, ConfigError> {
        Self::parse(config, read_bytes(config)?.as_deref())
    }

    /// The file's settings from `bytes`, as read from `config`; `None` is an absent file.
    fn parse(config: &Path, bytes: Option<&[u8]>) -> Result<Self, ConfigError> {
        let not_toml = || ConfigError::NotToml(config.to_path_buf());
        let text = std::str::from_utf8(bytes.unwrap_or_default()).map_err(|_| not_toml())?;
        Ok(Self {
            document: text.parse().map_err(|_| not_toml())?,
            exists: bytes.is_some(),
        })
    }

    /// Whether the file exists.
    #[must_use]
    pub const fn exists(&self) -> bool {
        self.exists
    }

    /// `key`'s value as the daemon will read it: `on` or `off`, the dollars as written, a token
    /// count, `none` for no floor ceiling, or `unreadable`.
    #[must_use]
    pub fn shown(&self, key: Key) -> String {
        match held(&self.document, key) {
            Held::Switch(true) => "on".to_owned(),
            Held::Switch(false) => "off".to_owned(),
            Held::Dollars(Some((_, text))) => text,
            Held::Dollars(None) | Held::Tokens(None) => "none".to_owned(),
            Held::Tokens(Some(count)) => count.to_string(),
            Held::Unreadable => "unreadable".to_owned(),
        }
    }

    /// Every way this file breaks its own start rules: unreadable keys first, then [`RULES`] in
    /// order. The daemon's journal can add a reason this cannot see; [`Edited::warnings`] names it.
    #[must_use]
    pub fn problems(&self) -> Vec<Problem> {
        let unreadable = Key::ALL
            .into_iter()
            .filter(|key| held(&self.document, *key) == Held::Unreadable)
            .map(Problem::Unreadable);
        let unmet = RULES
            .into_iter()
            .filter(|(key, needs)| {
                held(&self.document, *key).active() && !held(&self.document, *needs).satisfied()
            })
            .map(|(key, needs)| Problem::Needs { key, needs });
        unreadable.chain(unmet).collect()
    }

    /// Whether `key` is on, or above zero: what answers a start rule that asks for it.
    #[must_use]
    pub fn satisfies(&self, key: Key) -> bool {
        held(&self.document, key).satisfied()
    }

    /// The id of the workspace the file enrolls at `cwd`, if one does.
    #[must_use]
    pub fn enrolled_at(&self, cwd: &Path) -> Option<String> {
        workspace::id_at(&self.document, cwd)
    }

    /// The providers whose adapter table the file holds.
    #[must_use]
    pub fn adapters(&self) -> Vec<Provider> {
        Key::ALL
            .into_iter()
            .filter(|key| held(&self.document, *key) == Held::Switch(true))
            .filter_map(Key::adapter)
            .collect()
    }
}

/// What an edit did.
#[derive(Debug, PartialEq, Eq)]
pub struct Edited {
    /// Whether the file changed.
    pub wrote: bool,
    /// The per-turn reserves the edit took from above zero to zero.
    zeroed: Vec<Key>,
}

impl Edited {
    /// One warning per reserve the edit took to zero: the daemon refuses to start while an agent
    /// it has hired, or that agent's goal, has a ceiling in that unit, and only it can tell.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        self.zeroed
            .iter()
            .map(|key| {
                let unit = if *key == Key::PerTurnTokens {
                    "token"
                } else {
                    "dollar"
                };
                format!(
                    "{key} is now zero, and the daemon refuses to start while an agent it has \
                     hired, or that agent's goal, has a {unit} ceiling; set it above zero again \
                     before the next start if one does"
                )
            })
            .collect()
    }
}

/// Apply every change, then write the file, unless the result breaks the file's own start rules
/// or is byte for byte the file already there.
///
/// # Errors
///
/// A file that cannot be read, written or parsed, an adapter table [`ConfigError::Holds`] other
/// settings, and every [`Problem`] the result would have.
pub fn edit(config: &Path, changes: &[Change]) -> Result<Edited, ConfigError> {
    let mut draft = Draft::open(config)?;
    for change in changes {
        draft.apply(change)?;
    }
    draft.write()
}

/// The operator file in memory: read once, edited, and written at most once.
#[derive(Debug)]
pub struct Draft {
    config: PathBuf,
    previous: Option<Vec<u8>>,
    original: DocumentMut,
    settings: Settings,
}

impl Draft {
    /// Read the file; an absent one holds every default.
    ///
    /// # Errors
    ///
    /// A file that cannot be read or is not TOML.
    pub fn open(config: &Path) -> Result<Self, ConfigError> {
        let previous = read_bytes(config)?;
        let settings = Settings::parse(config, previous.as_deref())?;
        Ok(Self {
            config: config.to_path_buf(),
            previous,
            original: settings.document.clone(),
            settings,
        })
    }

    /// The file's bytes when it was opened; `None` when it did not exist.
    #[must_use]
    pub fn previous(&self) -> Option<&[u8]> {
        self.previous.as_deref()
    }

    /// The file as [`Self::write`] would write it.
    #[must_use]
    pub const fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Apply one change.
    ///
    /// # Errors
    ///
    /// A value the daemon would not read, or an adapter table [`ConfigError::Holds`] other
    /// settings.
    pub fn apply(&mut self, change: &Change) -> Result<(), ConfigError> {
        apply(&mut self.settings.document, change)
    }

    /// Append one workspace, checked as `dudes project add` checks it.
    ///
    /// # Errors
    ///
    /// The first refusal, in a fixed order.
    pub fn enroll(&mut self, new: &NewWorkspace) -> Result<(), WorkspaceError> {
        workspace::append(&self.config, &mut self.settings.document, new)
    }

    /// Write the draft, unless it breaks the file's own start rules or is byte for byte the file
    /// already there.
    ///
    /// # Errors
    ///
    /// Every [`Problem`] the draft has, or a write that failed.
    pub fn write(self) -> Result<Edited, ConfigError> {
        let problems = self.settings.problems();
        if !problems.is_empty() {
            return Err(ConfigError::Refused(problems));
        }
        let after = self.settings.document.to_string();
        let wrote = after != self.original.to_string();
        if wrote {
            workspace::replace(&self.config, after.as_bytes()).map_err(|source| {
                ConfigError::Io {
                    path: self.config.clone(),
                    source,
                }
            })?;
        }
        let zeroed = [Key::PerTurnUsd, Key::PerTurnTokens]
            .into_iter()
            .filter(|key| {
                held(&self.original, *key).satisfied()
                    && !held(&self.settings.document, *key).satisfied()
            })
            .collect();
        Ok(Edited { wrote, zeroed })
    }
}

fn read_bytes(config: &Path) -> Result<Option<Vec<u8>>, ConfigError> {
    workspace::read_bytes(config).map_err(|source| ConfigError::Io {
        path: config.to_path_buf(),
        source,
    })
}

/// What one key holds, with the daemon's default for an absent one.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Held {
    /// A switch, or an adapter's table.
    Switch(bool),
    /// Dollars and the text they were written as; `None` when absent.
    Dollars(Option<(Usd, String)>),
    /// A token count; `None` when absent.
    Tokens(Option<u64>),
    /// A value the daemon does not read as this key's.
    Unreadable,
}

impl Held {
    /// On, or present: what makes a rule ask for another key.
    fn active(&self) -> bool {
        matches!(self, Self::Switch(true) | Self::Dollars(Some(_)))
    }

    /// On, or above zero: what answers a rule.
    fn satisfied(&self) -> bool {
        match self {
            Self::Switch(on) => *on,
            Self::Dollars(Some((usd, _))) => *usd > Usd::ZERO,
            Self::Tokens(Some(count)) => *count > 0,
            Self::Dollars(None) | Self::Tokens(None) | Self::Unreadable => false,
        }
    }
}

fn held(document: &DocumentMut, key: Key) -> Held {
    let place = key.place();
    let mut table: &dyn TableLike = document.as_table();
    for name in place.path() {
        match table.get(name).map(Item::as_table_like) {
            None => return key.absent(),
            Some(None) => return Held::Unreadable,
            Some(Some(next)) => table = next,
        }
    }
    let Some(entry) = place.entry() else {
        return Held::Switch(true);
    };
    let Some(item) = table.get(entry) else {
        return key.absent();
    };
    let value = item.as_value();
    let read = match place {
        Place::Table(_) => Some(Held::Switch(true)),
        Place::Switch(..) => value.and_then(Value::as_bool).map(Held::Switch),
        Place::Dollars(..) => {
            value.and_then(|value| Some(Held::Dollars(Some((dollars(value)?, repr(value))))))
        }
        Place::Tokens(..) => value
            .and_then(Value::as_integer)
            .and_then(|count| u64::try_from(count).ok())
            .map(|count| Held::Tokens(Some(count))),
    };
    read.unwrap_or(Held::Unreadable)
}

/// The dollars a value holds, read as the daemon reads them: an integer or a float, taken from
/// its text exactly as written.
fn dollars(value: &Value) -> Option<Usd> {
    matches!(value, Value::Integer(_) | Value::Float(_))
        .then(|| repr(value))
        .and_then(|text| Usd::from_decimal(&text))
}

/// A number's text as written.
fn repr(value: &Value) -> String {
    match value {
        Value::Integer(number) => number.display_repr().into_owned(),
        Value::Float(number) => number.display_repr().into_owned(),
        _ => value.to_string(),
    }
}

fn apply(document: &mut DocumentMut, change: &Change) -> Result<(), ConfigError> {
    let unreadable = || ConfigError::Refused(vec![Problem::Unreadable(change.key)]);
    let refused = |problem| ConfigError::Refused(vec![problem]);
    let place = change.key.place();
    let path = place.path();
    match (place.entry(), &change.value) {
        (None, Some(_)) => table(document, path).map(drop).ok_or_else(unreadable),
        (None, None) => {
            if let Some(table) = existing(document, path, change.key).map_err(refused)? {
                let held: Vec<String> = table.iter().map(|(name, _)| format!("`{name}`")).collect();
                if !held.is_empty() {
                    return Err(ConfigError::Holds {
                        key: change.key,
                        held: held.join(", "),
                    });
                }
            }
            remove(document, change.key).map_err(refused)
        }
        (Some(_), None) => remove(document, change.key).map_err(refused),
        (Some(entry), Some(text)) => {
            let mut value: Value = text.parse().map_err(|_| unreadable())?;
            let parent = table(document, path).ok_or_else(unreadable)?;
            if let Some(Item::Value(old)) = parent.get_mut(entry) {
                *value.decor_mut() = old.decor().clone();
                *old = value;
            } else {
                parent.insert(entry, Item::Value(value));
            }
            Ok(())
        }
    }
}

fn remove(document: &mut DocumentMut, key: Key) -> Result<(), Problem> {
    let place = key.place();
    let (mut path, mut name) = match (place.entry(), place.path().split_last()) {
        (Some(entry), _) => (place.path(), entry),
        (None, Some((last, parents))) => (parents, *last),
        (None, None) => return Ok(()),
    };
    while let Some(table) = existing(document, path, key)? {
        table.remove(name);
        let enables_provider = Key::ALL.into_iter().any(|candidate| {
            matches!(candidate.place(), Place::Table(provider_path) if provider_path == path)
        });
        let Some((last, parents)) = path
            .split_last()
            .filter(|_| table.is_empty() && !enables_provider)
        else {
            break;
        };
        (path, name) = (parents, *last);
    }
    Ok(())
}

/// The table at `path`, each missing one created: implicit on the way, explicit at the end, so a
/// new one is written under its own `[a.b]` header. `None` when something on the way is a value.
fn table<'d>(document: &'d mut DocumentMut, path: &[&str]) -> Option<&'d mut dyn TableLike> {
    let mut table: &mut dyn TableLike = document.as_table_mut();
    for (depth, name) in path.iter().enumerate() {
        if table.get(name).is_none() {
            let mut new = Table::new();
            new.set_implicit(depth + 1 < path.len());
            table.insert(name, Item::Table(new));
        }
        table = table.get_mut(name)?.as_table_like_mut()?;
    }
    Some(table)
}

/// The table at `path` when it exists, or `key`'s [`Problem::Unreadable`] when something on the
/// way is a value.
fn existing<'d>(
    document: &'d mut DocumentMut,
    path: &[&str],
    key: Key,
) -> Result<Option<&'d mut dyn TableLike>, Problem> {
    let mut table: &mut dyn TableLike = document.as_table_mut();
    for name in path {
        match table.get_mut(name) {
            None => return Ok(None),
            Some(item) => {
                table = item.as_table_like_mut().ok_or(Problem::Unreadable(key))?;
            }
        }
    }
    Ok(Some(table))
}
