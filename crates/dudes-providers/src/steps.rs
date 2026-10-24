//! The steps of a managed turn as an engine's reader reports them: the tool calls it made and
//! how each ended. Every type here is in no vendor's vocabulary, and none of it has been checked
//! for secrets or cut to a size. The daemon does both before anything leaves it, and a result's
//! text goes no further than the daemon's own store.

use dudes_types::SessionId;
use serde_json::{Map, Value};

/// The most of a result's text that leaves the reader. It is far above what the daemon keeps, so
/// the daemon scrubs and checks a long result before it cuts it, and never keeps half a secret.
pub const MAX_RESULT_BYTES: usize = 64 * 1024;

/// Open calls kept at once. A call past this, or one that reuses the id of a call still open, is
/// not shown: it could never close, and would read as running until the turn ends.
pub(crate) const MAX_OPEN: usize = 256;
const MAX_ID_BYTES: usize = 128;
const MAX_RAW_ID_BYTES: usize = 1024;
const MAX_ARGS: usize = 16;
/// The most one nested argument flattens to. The rest is cut and marked, so one huge input
/// cannot multiply into a larger text before the daemon checks it.
pub(crate) const MAX_ARG_BYTES: usize = 64 * 1024;

/// What a step did, in the Floor's words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// Ran a shell command.
    Command,
    /// Read a file.
    Read,
    /// Changed part of a file.
    Edit,
    /// Wrote a whole file.
    Write,
    /// Searched files or names.
    Search,
    /// Fetched or searched the web.
    Web,
    /// Wrote or updated the plan.
    Plan,
    /// Handed work to a helper agent.
    Subagent,
    /// Called a tool from a connected server.
    Mcp,
    /// Anything else.
    Other,
}

impl StepKind {
    /// The kind's name on the wire.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Write => "write",
            Self::Search => "search",
            Self::Web => "web",
            Self::Plan => "plan",
            Self::Subagent => "subagent",
            Self::Mcp => "mcp",
            Self::Other => "other",
        }
    }
}

/// How far one plan item has come.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStatus {
    /// Not started.
    Pending,
    /// Being worked on.
    InProgress,
    /// Finished.
    Completed,
}

impl PlanStatus {
    /// The status's name on the wire.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// One line of a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanItem {
    /// What the item says.
    pub text: String,
    /// How far it has come.
    pub status: PlanStatus,
}

/// Text before and after an edit. A written file has no old text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditText {
    /// The text that was replaced.
    pub old: Option<String>,
    /// The text that replaced it.
    pub new: String,
}

/// One tool call, as the engine sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// The engine's id for the call.
    pub id: String,
    /// The call this one ran under, for a helper agent's work.
    pub parent: Option<String>,
    /// What it did.
    pub kind: StepKind,
    /// The tool's name.
    pub tool: String,
    /// The arguments that are text, in the order the engine sent them.
    pub args: Vec<(String, String)>,
    /// An edit's text, for an edit or a write.
    pub edit: Option<EditText>,
    /// The plan, for a plan step.
    pub plan: Vec<PlanItem>,
}

/// A change to the steps of a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepEvent {
    /// The engine started a call.
    Open {
        /// The session the line named.
        session: SessionId,
        /// The call.
        step: Step,
    },
    /// The engine reported how a call ended.
    Close {
        /// The session the line named.
        session: SessionId,
        /// The call's id.
        id: String,
        /// Whether the call failed.
        failed: bool,
        /// The command's exit status, when the engine said.
        exit: Option<i32>,
        /// The size of the result in bytes.
        result_bytes: u64,
        /// The start of the result's text, as the engine sent it and cut to a size the daemon
        /// can scrub whole. It goes to the daemon's result store and never to a browser.
        result: Option<String>,
    },
}

impl StepEvent {
    /// The session the event's line named.
    #[must_use]
    pub const fn session(&self) -> &SessionId {
        match self {
            Self::Open { session, .. } | Self::Close { session, .. } => session,
        }
    }
}

/// The arguments of a call that are text, in the order the engine sent them, minus the keys in
/// `skip`.
pub(crate) fn object_args(
    input: Option<&Map<String, Value>>,
    skip: &[&str],
) -> Vec<(String, String)> {
    input
        .into_iter()
        .flatten()
        .filter(|(key, _)| !skip.contains(&key.as_str()))
        .filter_map(|(key, value)| Some((key.clone(), arg(value)?)))
        .take(MAX_ARGS)
        .collect()
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// The id a step is shown under for an engine's id. An id of safe characters is its own. Any
/// other, such as `call_x|fc_y`, keeps its safe characters, turns the rest into `-`, and
/// ends in a hash of the original, so two engine ids never share one and the same id always maps
/// to the same step id.
pub(crate) fn step_id(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > MAX_RAW_ID_BYTES {
        return None;
    }
    if valid_id(raw) {
        return Some(raw.to_owned());
    }
    let hash = raw.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    let safe: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(MAX_ID_BYTES - 17)
        .collect();
    Some(format!("{safe}-{hash:016x}"))
}

/// An argument as text: a string as written, a list of strings one per line, and anything nested
/// as one `name: value` line per leaf. Strings keep their real newlines, never JSON escapes, so
/// the daemon's secret rules see the text as it was written.
pub(crate) fn arg(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Array(items) if items.iter().all(Value::is_string) => Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        other => {
            let mut lines = Vec::new();
            let mut room = MAX_ARG_BYTES;
            leaves(other, "", &mut lines, &mut room);
            (!lines.is_empty()).then(|| lines.join("\n"))
        }
    }
}

fn leaves(value: &Value, name: &str, lines: &mut Vec<String>, room: &mut usize) {
    if *room == 0 {
        return;
    }
    let mut push = |text: &str| {
        let line = if name.is_empty() {
            text.to_owned()
        } else {
            format!("{name}: {text}")
        };
        if line.len() > *room {
            lines.push(crate::stream::bound_to(&line, (*room).max(3)));
            *room = 0;
        } else {
            *room -= line.len() + 1;
            lines.push(line);
        }
    };
    match value {
        Value::Null => {}
        Value::String(text) => push(text),
        Value::Bool(_) | Value::Number(_) => push(&value.to_string()),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                leaves(item, &format!("{name}[{index}]"), lines, room);
            }
        }
        Value::Object(fields) => {
            for (key, item) in fields {
                let path = if name.is_empty() {
                    key.clone()
                } else {
                    format!("{name}.{key}")
                };
                leaves(item, &path, lines, room);
            }
        }
    }
}

/// A result's size in bytes and its text cut to what the daemon may read. No text is no result.
pub(crate) fn bounded_result(text: &str) -> (u64, Option<String>) {
    let kept = (!text.is_empty()).then(|| crate::stream::bound_to(text, MAX_RESULT_BYTES));
    (text.len() as u64, kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_safe_id_is_its_own_and_any_other_maps_to_a_stable_distinct_one() {
        assert_eq!(step_id("call_1-a").as_deref(), Some("call_1-a"));
        let bar = step_id("call_5X|fc_ac9a").unwrap();
        assert_eq!(bar, step_id("call_5X|fc_ac9a").unwrap());
        assert!(bar.starts_with("call_5X-fc_ac9a-") && valid_id(&bar));
        assert_eq!(bar.len(), "call_5X-fc_ac9a-".len() + 16);
        assert_ne!(step_id("a|b"), step_id("a/b"));
        assert!(valid_id(&step_id(&"é|".repeat(300)).unwrap()));
        assert_eq!((step_id(""), step_id(&"x".repeat(2000))), (None, None));
    }
}
