//! Codex's items as steps, from both of its streams.
//!
//! `codex exec --json` writes `item.started`, `item.updated` and `item.completed` lines with
//! `snake_case` fields, and the app-server writes `item/started` and `item/completed`
//! notifications with `camelCase` ones. The two carry the same items, so one mapping reads both from
//! the item as JSON, taking a field under either spelling.
//!
//! | Item type                            | Kind      | Input                          | Result            |
//! |--------------------------------------|-----------|--------------------------------|-------------------|
//! | `command_execution`                  | command   | `command`, and `cwd` if sent   | aggregated output |
//! | `file_change`, one step per path     | edit or write | `file_path`, a move target | none              |
//! | `mcp_tool_call`                      | mcp       | the call's `arguments`         | none              |
//! | `web_search`                         | web       | `query`                        | none              |
//! | `todo_list`, and a plan update       | plan      | the plan's items               | none              |
//! | any other type except replies        | other     | none                           | none              |
//!
//! A call opens on the item that starts it and closes on the item that completes it. The state
//! carried is the ids of the calls still open, so the same lines always give the same events.
//! A command with no text, or one that talks to a process already running, keeps no output. An item
//! that arrives only as completed opens and closes at once. Codex gives an exit code only for a
//! command, and no parent for any item.

use std::collections::{BTreeMap, BTreeSet};

use dudes_types::SessionId;
use serde_json::Value;

use crate::steps::{
    MAX_OPEN, PlanItem, PlanStatus, Step, StepEvent, StepKind, bounded_result, object_args, step_id,
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
    closed: BTreeSet<String>,
    plan_updates: u32,
    last_plan: Vec<PlanItem>,
}

impl LiveSteps {
    pub(super) fn started(&mut self, session: &SessionId, item: &Value) -> Vec<StepEvent> {
        let Some(id) = item_id(item) else {
            return Vec::new();
        };
        if self.open.len() >= MAX_OPEN || self.open.contains_key(&id) {
            return Vec::new();
        }
        let steps = steps(&id, item);
        if steps.is_empty() {
            return Vec::new();
        }
        let plan = steps
            .first()
            .map(|step| step.plan.clone())
            .unwrap_or_default();
        self.open.insert(
            id,
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
        let Some(open) = self.open.get_mut(&id) else {
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
        if self.closed.contains(&id) {
            return Vec::new();
        }
        let (mut events, ids) = if self.open.contains_key(&id) {
            let events = self.updated(session, item);
            let open = self.open.remove(&id);
            (events, open.map(|open| open.ids).unwrap_or_default())
        } else {
            let steps = steps(&id, item);
            let ids = steps.iter().map(|step| step.id.clone()).collect();
            (opens(session, steps), ids)
        };
        if self.closed.len() < MAX_OPEN {
            self.closed.insert(id);
        }
        let (exit, bytes, result) = outcome(item);
        events.extend(closes(
            session,
            ids,
            failed(item),
            exit,
            Some((bytes, result)),
        ));
        events
    }

    /// The app-server's plan for the turn, restated in full. A plan has no running phase, so each
    /// restatement opens and closes at once, and one that repeats the last is not a step.
    pub(super) fn plan(&mut self, session: &SessionId, items: Vec<PlanItem>) -> Vec<StepEvent> {
        if self.plan_updates > 0 && items == self.last_plan {
            return Vec::new();
        }
        self.plan_updates += 1;
        let id = format!("plan-{}", self.plan_updates);
        self.last_plan.clone_from(&items);
        let mut events = vec![StepEvent::Open {
            session: session.clone(),
            step: plan_step(id.clone(), "plan", items),
        }];
        events.extend(closes(session, vec![id], false, None, None));
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

/// A field under its `snake_case` or `camelCase` spelling.
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

fn item_id(item: &Value) -> Option<String> {
    item.get("id").and_then(Value::as_str).and_then(step_id)
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
            let mut args: Vec<(String, String)> = command_text(item)
                .map(|command| vec![("command".to_owned(), command)])
                .unwrap_or_default();
            if let Some(cwd) = text(item, "cwd", "cwd").filter(|cwd| !cwd.is_empty()) {
                args.push(("cwd".to_owned(), cwd.to_owned()));
            }
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
            let mut args = vec![("file_path".to_owned(), path.to_owned())];
            if let Some(target) = move_target(change) {
                args.push(("move_path".to_owned(), target.to_owned()));
            }
            Step {
                id: if single {
                    id.to_owned()
                } else {
                    format!("{id}-{index}")
                },
                parent: None,
                kind,
                tool: tool.to_owned(),
                args,
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

/// Where a change moves the file to. The daemon reads `move_path` as a path, so a move into a
/// sensitive folder is withheld like an edit of a file there.
fn move_target(change: &Value) -> Option<&str> {
    let kind = change.get("kind")?;
    text(kind, "move_path", "movePath").filter(|target| !target.is_empty())
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
    if !keeps_output(item) {
        return (failed_exit(item), 0, None);
    }
    let exit = failed_exit(item);
    let (bytes, result) =
        bounded_result(text(item, "aggregated_output", "aggregatedOutput").unwrap_or_default());
    (exit, bytes, result)
}

fn failed_exit(item: &Value) -> Option<i32> {
    failed(item)
        .then(|| field(item, "exit_code", "exitCode").and_then(Value::as_i64))
        .flatten()
        .and_then(|code| i32::try_from(code).ok())
}

/// Whether a command's output is the output of the command the step names. A step with no
/// command text, or one that talks to a process already running, has output the daemon cannot
/// check against a command, so its text is not passed on.
fn keeps_output(item: &Value) -> bool {
    let named = command_text(item).is_some_and(|command| !command.trim().is_empty());
    let interaction = matches!(
        text(item, "source", "source"),
        Some("unifiedExecInteraction" | "unified_exec_interaction")
    );
    named && !interaction
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::stream::TurnStream;
    use crate::steps::MAX_RESULT_BYTES;
    use crate::stream::TurnReader;

    const THREAD: &str = "01a0e1d9-84d5-7bc2-8567-d32735103d71";

    fn run(lines: &[String]) -> Vec<StepEvent> {
        let mut stream = TurnStream::new();
        stream.line(&format!(
            r#"{{"type":"thread.started","thread_id":"{THREAD}"}}"#
        ));
        lines
            .iter()
            .flat_map(|line| stream.line(line).steps)
            .collect()
    }

    fn started(item: &str) -> String {
        format!(r#"{{"type":"item.started","item":{item}}}"#)
    }

    fn completed(item: &str) -> String {
        format!(r#"{{"type":"item.completed","item":{item}}}"#)
    }

    fn open(event: &StepEvent) -> &Step {
        let StepEvent::Open { step, .. } = event else {
            panic!("an open step expected, got {event:?}");
        };
        step
    }

    fn close(event: &StepEvent) -> (&str, bool, Option<i32>, u64, Option<&str>) {
        let StepEvent::Close {
            id,
            failed,
            exit,
            result_bytes,
            result,
            ..
        } = event
        else {
            panic!("a close expected, got {event:?}");
        };
        (id, *failed, *exit, *result_bytes, result.as_deref())
    }

    #[test]
    fn a_captured_command_opens_and_closes_with_its_output() {
        let fixture = include_str!("../../tests/fixtures/codex/c2-command.stdout");
        let mut stream = TurnStream::new();
        let events: Vec<StepEvent> = fixture
            .lines()
            .flat_map(|line| stream.line(line).steps)
            .collect();
        let [opened, closed] = &events[..] else {
            panic!("an open and a close expected, got {events:?}");
        };
        let step = open(opened);
        assert_eq!(step.id, "item_1");
        assert_eq!(step.kind, StepKind::Command);
        assert_eq!(
            step.args,
            [("command".to_owned(), "/bin/zsh -lc 'echo ok'".to_owned())]
        );
        assert_eq!(close(closed), ("item_1", false, None, 3, Some("ok\n")));
    }

    #[test]
    fn a_real_run_with_a_failed_command_and_a_two_path_patch_reads_as_steps() {
        let fixture = include_str!("../../tests/fixtures/codex/c4-file-change.stdout");
        let mut stream = TurnStream::new();
        let events: Vec<StepEvent> = fixture
            .lines()
            .flat_map(|line| stream.line(line).steps)
            .collect();
        let summary: Vec<String> = events
            .iter()
            .map(|event| match event {
                StepEvent::Open { step, .. } => {
                    format!(
                        "open {} {:?} {} {:?}",
                        step.id, step.kind, step.tool, step.args
                    )
                }
                StepEvent::Close {
                    id,
                    failed,
                    exit,
                    result_bytes,
                    result,
                    ..
                } => format!("close {id} {failed} {exit:?} {result_bytes} {result:?}"),
            })
            .collect();
        assert_eq!(
            summary,
            [
                r#"open item_1 Command shell [("command", "/bin/zsh -lc ls")]"#,
                r#"close item_1 false None 18 Some("main.rs\nnotes.txt\n")"#,
                r#"open item_2 Command shell [("command", "/bin/zsh -lc \"pwd; rg --files --hidden -g AGENTS.md -g '\"'! .git'\"'; cat notes.txt; git status --short\"")]"#,
                r#"close item_2 true Some(128) 87 Some("/work\nhello\nworld\nfatal: not a git repository (or any of the parent directories): .git\n")"#,
                r#"open item_3-0 Write add [("file_path", "/work/created.txt")]"#,
                r#"open item_3-1 Edit update [("file_path", "/work/notes.txt")]"#,
                "close item_3-0 false None 0 None",
                "close item_3-1 false None 0 None",
                r#"open item_4 Command shell [("command", "/bin/zsh -lc 'cat notes.txt created.txt'")]"#,
                r#"close item_4 false None 16 Some("hello\nEarth\nnew\n")"#,
            ]
        );
    }

    #[test]
    fn a_failed_command_closes_failed_with_its_exit_status() {
        let events = run(&[
            started(
                r#"{"id":"item_2","type":"command_execution","command":"false","aggregated_output":"","exit_code":null,"status":"in_progress"}"#,
            ),
            completed(
                r#"{"id":"item_2","type":"command_execution","command":"false","aggregated_output":"boom","exit_code":2,"status":"failed"}"#,
            ),
        ]);
        assert_eq!(
            close(&events[1]),
            ("item_2", true, Some(2), 4, Some("boom"))
        );
    }

    #[test]
    fn a_long_output_is_cut_at_the_reader_limit() {
        let output = "o".repeat(MAX_RESULT_BYTES * 2);
        let events = run(&[
            started(
                r#"{"id":"item_3","type":"command_execution","command":"yes","status":"in_progress"}"#,
            ),
            completed(&format!(
                r#"{{"id":"item_3","type":"command_execution","command":"yes","aggregated_output":"{output}","exit_code":0,"status":"completed"}}"#
            )),
        ]);
        let (_, failed, _, bytes, result) = close(&events[1]);
        assert!(!failed);
        assert_eq!(bytes, output.len() as u64);
        let result = result.unwrap();
        assert!(result.len() <= MAX_RESULT_BYTES && result.ends_with('…'));
    }

    #[test]
    fn a_file_change_is_one_step_per_path_by_change_kind() {
        let changes = r#""changes":[{"path":"/repo/new.rs","kind":"add"},{"path":"/repo/a.rs","kind":"update"}]"#;
        let events = run(&[
            started(&format!(
                r#"{{"id":"item_4","type":"file_change",{changes},"status":"in_progress"}}"#
            )),
            completed(&format!(
                r#"{{"id":"item_4","type":"file_change",{changes},"status":"completed"}}"#
            )),
        ]);
        let [first, second, done_first, done_second] = &events[..] else {
            panic!("two opens and two closes expected, got {events:?}");
        };
        let (first, second) = (open(first), open(second));
        assert_eq!(
            (first.id.as_str(), first.kind),
            ("item_4-0", StepKind::Write)
        );
        assert_eq!(
            (second.id.as_str(), second.kind),
            ("item_4-1", StepKind::Edit)
        );
        assert_eq!(
            first.args,
            [("file_path".to_owned(), "/repo/new.rs".to_owned())]
        );
        assert_eq!(first.edit, None);
        assert_eq!(close(done_first).0, "item_4-0");
        assert_eq!(close(done_second), ("item_4-1", false, None, 0, None));
    }

    #[test]
    fn a_failed_patch_closes_failed() {
        let item = r#""type":"file_change","changes":[{"path":"/repo/a.rs","kind":"update"}]"#;
        let events = run(&[
            started(&format!(
                r#"{{"id":"item_5",{item},"status":"in_progress"}}"#
            )),
            completed(&format!(r#"{{"id":"item_5",{item},"status":"failed"}}"#)),
        ]);
        assert_eq!(open(&events[0]).id, "item_5");
        assert!(close(&events[1]).1);
    }

    #[test]
    fn mcp_web_and_unknown_items_get_their_kinds_and_no_result() {
        let events = run(&[
            started(
                r#"{"id":"item_6","type":"mcp_tool_call","server":"docs","tool":"find","arguments":{"topic":"rust"},"status":"in_progress"}"#,
            ),
            completed(
                r#"{"id":"item_6","type":"mcp_tool_call","server":"docs","tool":"find","arguments":{"topic":"rust"},"result":{"content":[]},"status":"completed"}"#,
            ),
            started(r#"{"id":"item_7","type":"web_search","query":"rust lifetimes"}"#),
            completed(r#"{"id":"item_7","type":"web_search","query":"rust lifetimes"}"#),
            started(r#"{"id":"item_8","type":"collab_tool_call","status":"in_progress"}"#),
            completed(r#"{"id":"item_8","type":"collab_tool_call","status":"completed"}"#),
        ]);
        let kinds: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                StepEvent::Open { step, .. } => Some((step.kind, step.tool.as_str())),
                StepEvent::Close { .. } => None,
            })
            .collect();
        assert_eq!(
            kinds,
            [
                (StepKind::Mcp, "mcp__docs__find"),
                (StepKind::Web, "web_search"),
                (StepKind::Other, "collab_tool_call"),
            ]
        );
        assert_eq!(
            open(&events[0]).args,
            [("topic".to_owned(), "rust".to_owned())]
        );
        assert_eq!(
            open(&events[2]).args,
            [("query".to_owned(), "rust lifetimes".to_owned())]
        );
        assert!(events.iter().all(|event| match event {
            StepEvent::Close {
                result,
                result_bytes,
                ..
            } => result.is_none() && *result_bytes == 0,
            StepEvent::Open { .. } => true,
        }));
    }

    #[test]
    fn replies_and_reasoning_are_not_steps() {
        let events = run(&[
            completed(r#"{"id":"item_0","type":"reasoning","text":"thinking"}"#),
            started(r#"{"id":"item_9","type":"agent_message","text":""}"#),
            completed(r#"{"id":"item_9","type":"agent_message","text":"done"}"#),
        ]);
        assert!(events.is_empty());
    }

    #[test]
    fn a_todo_list_is_a_plan_that_reads_as_a_new_step_when_it_changes() {
        let list = |first: bool, second: bool| {
            format!(
                r#""id":"item_10","type":"todo_list","items":[{{"text":"Write it","completed":{first}}},{{"text":"Test it","completed":{second}}}]"#
            )
        };
        let events = run(&[
            started(&format!("{{{}}}", list(false, false))),
            format!(
                r#"{{"type":"item.updated","item":{{{}}}}}"#,
                list(true, false)
            ),
            format!(
                r#"{{"type":"item.updated","item":{{{}}}}}"#,
                list(true, false)
            ),
            completed(&format!("{{{}}}", list(true, true))),
        ]);
        let ids: Vec<_> = events
            .iter()
            .map(|event| match event {
                StepEvent::Open { step, .. } => format!("open {}", step.id),
                StepEvent::Close { id, .. } => format!("close {id}"),
            })
            .collect();
        assert_eq!(
            ids,
            [
                "open item_10",
                "close item_10",
                "open item_10-r1",
                "close item_10-r1",
                "open item_10-r2",
                "close item_10-r2",
            ]
        );
        let newest = open(&events[4]);
        assert_eq!(newest.kind, StepKind::Plan);
        assert_eq!(
            newest
                .plan
                .iter()
                .map(|item| item.status)
                .collect::<Vec<_>>(),
            [PlanStatus::Completed, PlanStatus::Completed]
        );
    }

    #[test]
    fn a_command_given_as_a_list_reads_as_the_script_it_runs() {
        let item = |command: &str| serde_json::json!({"command": serde_json::from_str::<Value>(command).unwrap()});
        assert_eq!(
            command_text(&item(r#"["bash","-lc","cat ~/.aws/credentials"]"#)).as_deref(),
            Some("cat ~/.aws/credentials")
        );
        assert_eq!(
            command_text(&item(r#"["git","status","--short"]"#)).as_deref(),
            Some("git status --short")
        );
    }

    #[test]
    fn items_that_only_arrive_completed_open_and_close_together() {
        let events = run(&[
            completed(
                r#"{"id":"item_11","type":"file_change","changes":[{"path":"/repo/a.rs","kind":"update"}],"status":"completed"}"#,
            ),
            completed(r#"{"id":"item_12","type":"web_search","query":"rust lifetimes"}"#),
        ]);
        let [patch, patched, search, searched] = &events[..] else {
            panic!("two opens and two closes expected, got {events:?}");
        };
        assert_eq!(
            (open(patch).id.as_str(), open(patch).kind),
            ("item_11", StepKind::Edit)
        );
        assert_eq!(close(patched), ("item_11", false, None, 0, None));
        assert_eq!(
            (open(search).tool.as_str(), open(search).kind),
            ("web_search", StepKind::Web)
        );
        assert_eq!(close(searched), ("item_12", false, None, 0, None));
    }

    #[test]
    fn a_repeated_completion_is_not_a_second_step() {
        let item = r#"{"id":"item_16","type":"web_search","query":"rust"}"#;
        let events = run(&[started(item), completed(item), completed(item)]);
        assert_eq!(events.len(), 2, "one open and one close, got {events:?}");
    }

    #[test]
    fn a_move_target_rides_with_the_step_under_a_path_key() {
        let events = run(&[started(
            r#"{"id":"item_13","type":"file_change","changes":[{"path":"/repo/a.rs","kind":{"type":"update","move_path":"/work/me/.ssh/a.rs"}}]}"#,
        )]);
        assert_eq!(
            open(&events[0]).args,
            [
                ("file_path".to_owned(), "/repo/a.rs".to_owned()),
                ("move_path".to_owned(), "/work/me/.ssh/a.rs".to_owned()),
            ]
        );
    }

    #[test]
    fn a_command_with_no_text_or_that_talks_to_a_running_process_keeps_no_output() {
        let events = run(&[
            started(r#"{"id":"item_14","type":"command_execution","status":"in_progress"}"#),
            completed(
                r#"{"id":"item_14","type":"command_execution","command":"","aggregated_output":"PRIVATE","exit_code":0,"status":"completed"}"#,
            ),
            started(
                r#"{"id":"item_15","type":"command_execution","command":"ls","status":"in_progress"}"#,
            ),
            completed(
                r#"{"id":"item_15","type":"command_execution","command":"ls","source":"unifiedExecInteraction","aggregated_output":"PRIVATE","exit_code":0,"status":"completed"}"#,
            ),
        ]);
        assert_eq!(close(&events[1]), ("item_14", false, None, 0, None));
        assert_eq!(close(&events[3]), ("item_15", false, None, 0, None));
    }

    #[test]
    fn a_command_carries_its_working_directory() {
        let events = run(&[started(
            r#"{"id":"item_16","type":"command_execution","command":"cat *","cwd":"/work/me/.ssh"}"#,
        )]);
        assert_eq!(
            open(&events[0]).args,
            [
                ("command".to_owned(), "cat *".to_owned()),
                ("cwd".to_owned(), "/work/me/.ssh".to_owned()),
            ]
        );
    }

    #[test]
    fn no_session_means_no_steps() {
        let mut stream = TurnStream::new();
        let read = stream.line(&started(
            r#"{"id":"item_1","type":"command_execution","command":"ls"}"#,
        ));
        assert!(read.steps.is_empty());
    }
}
