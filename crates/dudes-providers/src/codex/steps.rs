//! Codex's items as steps, from both of its streams.
//!
//! `codex exec --json` writes `item.started`, `item.updated` and `item.completed` lines with
//! snake_case fields, and the app-server writes `item/started` and `item/completed`
//! notifications with camelCase ones. The two carry the same items, so one mapping reads both from
//! the item as JSON, taking a field under either spelling.
//!
//! | Item type                            | Kind      | Input                          | Result            |
//! |--------------------------------------|-----------|--------------------------------|-------------------|
//! | `command_execution`                  | command   | `command`                      | aggregated output |
//! | `file_change`, one step per path     | edit or write | `file_path`                | none              |
//! | `mcp_tool_call`                      | mcp       | the call's `arguments`         | none              |
//! | `web_search`                         | web       | `query`                        | none              |
//! | `todo_list`, and a plan update       | plan      | the plan's items               | none              |
//! | any other type except replies        | other     | none                           | none              |
//!
//! A call opens on the item that starts it and closes on the item that completes it. The state
//! carried is the ids of the calls still open, so the same lines always give the same events.
//! Codex gives an exit code only for a command, and no parent for any item.

use std::collections::BTreeMap;

use dudes_types::SessionId;
use serde_json::Value;

use crate::steps::{
    MAX_OPEN, PlanItem, PlanStatus, Step, StepEvent, StepKind, bounded_result, object_args,
    valid_id,
};

const MAX_PATHS: usize = 64;
const MAX_PLAN_ITEMS: usize = 64;
const MAX_TOOL_BYTES: usize = 128;

/// Whether an item type is worth reading as a step. Replies and reasoning are text the turn
/// already shows.
pub(super) fn is_step(kind: &str) -> bool {
    !kind.is_empty()
        && !matches!(
            kind,
            "agent_message"
                | "agentMessage"
                | "reasoning"
                | "error"
                | "user_message"
                | "userMessage"
                | "plan"
        )
}

#[derive(Debug)]
struct Open {
    ids: Vec<String>,
    plan: Vec<PlanItem>,
    revision: u32,
}

#[derive(Debug, Default)]
pub(super) struct LiveSteps {
    open: BTreeMap<String, Open>,
    plan_updates: u32,
}

impl LiveSteps {
    pub(super) fn started(&mut self, session: &SessionId, item: &Value) -> Vec<StepEvent> {
        let Some(id) = item_id(item) else {
            return Vec::new();
        };
        if self.open.len() >= MAX_OPEN || self.open.contains_key(id) {
            return Vec::new();
        }
        let steps = steps(id, item);
        if steps.is_empty() {
            return Vec::new();
        }
        let plan = steps
            .first()
            .map(|step| step.plan.clone())
            .unwrap_or_default();
        self.open.insert(
            id.to_owned(),
            Open {
                ids: steps.iter().map(|step| step.id.clone()).collect(),
                plan,
                revision: 0,
            },
        );
        opens(session, steps)
    }

    /// A to-do list restated in full. The plan reads as a new step when its items changed.
    pub(super) fn updated(&mut self, session: &SessionId, item: &Value) -> Vec<StepEvent> {
        let Some(id) = item_id(item) else {
            return Vec::new();
        };
        let Some(open) = self.open.get_mut(id) else {
            return Vec::new();
        };
        if !is_todo(item) {
            return Vec::new();
        }
        let items = todo_items(item);
        if items == open.plan {
            return Vec::new();
        }
        open.revision += 1;
        let next = format!("{id}-r{}", open.revision);
        let mut events = closes(session, std::mem::take(&mut open.ids), false, None, None);
        open.ids = vec![next.clone()];
        open.plan.clone_from(&items);
        events.push(StepEvent::Open {
            session: session.clone(),
            step: plan_step(next, "todo_list", items),
        });
        events
    }

    pub(super) fn completed(&mut self, session: &SessionId, item: &Value) -> Vec<StepEvent> {
        let Some(id) = item_id(item) else {
            return Vec::new();
        };
        if !self.open.contains_key(id) {
            return Vec::new();
        }
        let mut events = self.updated(session, item);
        let Some(open) = self.open.remove(id) else {
            return events;
        };
        let failed = failed(item);
        let (exit, bytes, result) = outcome(item);
        events.extend(closes(
            session,
            open.ids,
            failed,
            exit,
            Some((bytes, result)),
        ));
        events
    }

    /// The app-server's plan for the turn, restated in full. Each restatement is a step, and the
    /// one before it is over.
    pub(super) fn plan(&mut self, session: &SessionId, items: Vec<PlanItem>) -> Vec<StepEvent> {
        let mut events = Vec::new();
        if self.plan_updates > 0 {
            events.extend(closes(
                session,
                vec![format!("plan-{}", self.plan_updates)],
                false,
                None,
                None,
            ));
        }
        self.plan_updates += 1;
        events.push(StepEvent::Open {
            session: session.clone(),
            step: plan_step(format!("plan-{}", self.plan_updates), "plan", items),
        });
        events
    }
}

/// The plan items of an app-server `turn/plan/updated` notification.
pub(super) fn plan_items(plan: &[Value]) -> Vec<PlanItem> {
    plan.iter()
        .take(MAX_PLAN_ITEMS)
        .filter_map(|entry| {
            let text = entry.get("step").and_then(Value::as_str)?;
            let status = match entry.get("status").and_then(Value::as_str) {
                Some("completed") => PlanStatus::Completed,
                Some("inProgress" | "in_progress") => PlanStatus::InProgress,
                _ => PlanStatus::Pending,
            };
            Some(PlanItem {
                text: text.to_owned(),
                status,
            })
        })
        .collect()
}

fn opens(session: &SessionId, steps: Vec<Step>) -> Vec<StepEvent> {
    steps
        .into_iter()
        .map(|step| StepEvent::Open {
            session: session.clone(),
            step,
        })
        .collect()
}

fn closes(
    session: &SessionId,
    ids: Vec<String>,
    failed: bool,
    exit: Option<i32>,
    result: Option<(u64, Option<String>)>,
) -> Vec<StepEvent> {
    let (result_bytes, result) = result.unwrap_or_default();
    ids.into_iter()
        .map(|id| StepEvent::Close {
            session: session.clone(),
            id,
            failed,
            exit,
            result_bytes,
            result: result.clone(),
        })
        .collect()
}

/// A field under its snake_case or camelCase spelling.
fn field<'a>(item: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    item.get(snake)
        .or_else(|| item.get(camel))
        .filter(|value| !value.is_null())
}

fn text<'a>(item: &'a Value, snake: &str, camel: &str) -> Option<&'a str> {
    field(item, snake, camel).and_then(Value::as_str)
}

fn item_type(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or_default()
}

fn item_id(item: &Value) -> Option<&str> {
    item.get("id")
        .and_then(Value::as_str)
        .filter(|id| valid_id(id))
}

fn is_todo(item: &Value) -> bool {
    item_type(item) == "todo_list"
}

fn steps(id: &str, item: &Value) -> Vec<Step> {
    let one = |kind, tool: &str, args: Vec<(String, String)>| Step {
        id: id.to_owned(),
        parent: None,
        kind,
        tool: tool.to_owned(),
        args,
        edit: None,
        plan: Vec::new(),
    };
    match item_type(item) {
        "command_execution" | "commandExecution" => {
            let args = command_text(item)
                .map(|command| vec![("command".to_owned(), command)])
                .unwrap_or_default();
            vec![one(StepKind::Command, "shell", args)]
        }
        "file_change" | "fileChange" => file_steps(id, item),
        "mcp_tool_call" | "mcpToolCall" => {
            let server = text(item, "server", "server").unwrap_or_default();
            let tool = text(item, "tool", "tool").unwrap_or_default();
            let arguments = item.get("arguments").and_then(Value::as_object);
            let args =
                arguments.map_or_else(Vec::new, |arguments| object_args(Some(arguments), &[]));
            let name = crate::stream::bound_to(&format!("mcp__{server}__{tool}"), MAX_TOOL_BYTES);
            vec![one(StepKind::Mcp, &name, args)]
        }
        "web_search" | "webSearch" => {
            let args = text(item, "query", "query")
                .map(|query| vec![("query".to_owned(), query.to_owned())])
                .unwrap_or_default();
            vec![one(StepKind::Web, "web_search", args)]
        }
        "todo_list" => vec![plan_step(id.to_owned(), "todo_list", todo_items(item))],
        other if is_step(other) => {
            let tool = crate::stream::bound_to(other, MAX_TOOL_BYTES);
            vec![one(StepKind::Other, &tool, Vec::new())]
        }
        _ => Vec::new(),
    }
}

fn plan_step(id: String, tool: &str, plan: Vec<PlanItem>) -> Step {
    Step {
        id,
        parent: None,
        kind: StepKind::Plan,
        tool: tool.to_owned(),
        args: Vec::new(),
        edit: None,
        plan,
    }
}

/// The command as one string the daemon's sensitive-path check reads word by word. The exec
/// stream sends a string. A list of words that is a shell run with `-c`, as in
/// `["bash", "-lc", "cat ~/.aws/credentials"]`, becomes the script it runs, and any other list its
/// words joined by spaces.
fn command_text(item: &Value) -> Option<String> {
    match item.get("command")? {
        Value::String(command) => Some(command.clone()),
        Value::Array(words) => {
            let words: Vec<&str> = words.iter().filter_map(Value::as_str).collect();
            match words.as_slice() {
                [shell, flag, script]
                    if shell.rsplit('/').next().is_some_and(is_shell)
                        && flag.starts_with('-')
                        && flag.ends_with('c') =>
                {
                    Some((*script).to_owned())
                }
                [] => None,
                words => Some(words.join(" ")),
            }
        }
        _ => None,
    }
}

fn is_shell(name: &str) -> bool {
    matches!(name, "bash" | "sh" | "zsh" | "dash" | "fish")
}

fn file_steps(id: &str, item: &Value) -> Vec<Step> {
    let changes: Vec<&Value> = item
        .get("changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|change| change.get("path").and_then(Value::as_str).is_some())
        .take(MAX_PATHS)
        .collect();
    let single = changes.len() == 1;
    changes
        .into_iter()
        .enumerate()
        .map(|(index, change)| {
            let path = change
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (kind, tool) = match change_kind(change) {
                "add" => (StepKind::Write, "add"),
                "delete" => (StepKind::Edit, "delete"),
                _ => (StepKind::Edit, "update"),
            };
            Step {
                id: if single {
                    id.to_owned()
                } else {
                    format!("{id}-{index}")
                },
                parent: None,
                kind,
                tool: tool.to_owned(),
                args: vec![("file_path".to_owned(), path.to_owned())],
                edit: None,
                plan: Vec::new(),
            }
        })
        .collect()
}

/// `add`, `update` or `delete`, as a word in the exec stream and as a tagged object in the
/// app-server's.
fn change_kind(change: &Value) -> &str {
    match change.get("kind") {
        Some(Value::String(kind)) => kind,
        Some(Value::Object(kind)) => kind.get("type").and_then(Value::as_str).unwrap_or_default(),
        _ => "",
    }
}

fn todo_items(item: &Value) -> Vec<PlanItem> {
    item.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_PLAN_ITEMS)
        .filter_map(|entry| {
            let text = entry.get("text").and_then(Value::as_str)?;
            let done = entry.get("completed").and_then(Value::as_bool) == Some(true);
            Some(PlanItem {
                text: text.to_owned(),
                status: if done {
                    PlanStatus::Completed
                } else {
                    PlanStatus::Pending
                },
            })
        })
        .collect()
}

fn failed(item: &Value) -> bool {
    let status = item.get("status").and_then(Value::as_str);
    let errored = item.get("error").is_some_and(|error| !error.is_null());
    let exit = field(item, "exit_code", "exitCode").and_then(Value::as_i64);
    matches!(status, Some("failed" | "declined")) || errored || exit.is_some_and(|code| code != 0)
}

/// The exit status of a failed command and the size and start of its output.
fn outcome(item: &Value) -> (Option<i32>, u64, Option<String>) {
    if !matches!(item_type(item), "command_execution" | "commandExecution") {
        return (None, 0, None);
    }
    let exit = failed(item)
        .then(|| field(item, "exit_code", "exitCode").and_then(Value::as_i64))
        .flatten()
        .and_then(|code| i32::try_from(code).ok());
    let (bytes, result) =
        bounded_result(text(item, "aggregated_output", "aggregatedOutput").unwrap_or_default());
    (exit, bytes, result)
}
