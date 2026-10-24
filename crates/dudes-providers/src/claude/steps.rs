//! Tool calls and their results, read from Claude's typed lines.
//!
//! A call opens on the `assistant` line that carries its `tool_use` block, whole, and closes on
//! the `user` line that carries its `tool_result`. The only carried state is the ids of the calls
//! still open, so the same lines always give the same events. A result's text leaves here only as
//! a bounded start for the daemon to scrub and keep. It is never sent on to a browser as it is.

use dudes_proto::providers::claude::ContentBlock;
use dudes_types::SessionId;
use serde_json::Value;

use super::{Line, StreamLine};
use crate::steps::{EditText, MAX_RESULT_BYTES, PlanItem, PlanStatus, Step, StepEvent, StepKind};

/// Open calls kept at once. A call past this, or one that reuses the id of a call still open, is
/// not shown: it could never close, and would read as running until the turn ends.
const MAX_OPEN: usize = 256;
const MAX_ID_BYTES: usize = 128;
const MAX_ARGS: usize = 16;
const MAX_PLAN_ITEMS: usize = 64;
/// The most one nested argument flattens to. The rest is cut and marked, so one huge input
/// cannot multiply into a larger text before the daemon checks it.
const MAX_ARG_BYTES: usize = 64 * 1024;

#[derive(Debug, Default)]
pub(super) struct LiveSteps {
    open: Vec<String>,
}

impl LiveSteps {
    pub(super) fn read(&mut self, line: &Line, announced: Option<&SessionId>) -> Vec<StepEvent> {
        let Some(session) = line
            .session_id()
            .filter(|id| !id.is_empty() && announced.is_some_and(|a| a.as_str() == *id))
            .map(SessionId::new)
        else {
            return Vec::new();
        };
        match &line.body {
            StreamLine::Assistant(message)
                if message.message.role == "assistant" && message.is_synthetic != Some(true) =>
            {
                message
                    .message
                    .content
                    .iter()
                    .filter(|block| block.kind == "tool_use")
                    .filter_map(|block| {
                        let step = step(block, message.parent_tool_use_id.as_deref())?;
                        if self.open.len() >= MAX_OPEN || self.open.contains(&step.id) {
                            return None;
                        }
                        self.open.push(step.id.clone());
                        Some(StepEvent::Open {
                            session: session.clone(),
                            step,
                        })
                    })
                    .collect()
            }
            StreamLine::User(message) => message
                .message
                .content
                .iter()
                .filter(|block| block.kind == "tool_result")
                .filter_map(|block| {
                    let id = block.tool_use_id.as_deref()?;
                    let at = self.open.iter().position(|open| open == id)?;
                    self.open.remove(at);
                    let failed = block.is_error == Some(true);
                    let (bytes, head, result) = result_text(block.content.as_ref());
                    Some(StepEvent::Close {
                        session: session.clone(),
                        id: id.to_owned(),
                        failed,
                        exit: failed.then(|| exit_code(&head)).flatten(),
                        result_bytes: bytes,
                        result,
                    })
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

fn step(block: &ContentBlock, parent: Option<&str>) -> Option<Step> {
    let id = block.id.as_deref().filter(|id| valid_id(id))?;
    let tool = block.name.as_deref().filter(|name| !name.is_empty())?;
    let input = block.input.as_ref().and_then(Value::as_object);
    let text = |key: &str| {
        input
            .and_then(|input| input.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let kind = kind(tool);
    let (edit, skip): (_, &[&str]) = match tool {
        "Edit" => (
            text("new_string").map(|new| EditText {
                old: text("old_string"),
                new,
            }),
            &["old_string", "new_string"],
        ),
        "Write" => (
            text("content").map(|new| EditText { old: None, new }),
            &["content"],
        ),
        "TodoWrite" => (None, &["todos"]),
        _ => (None, &[]),
    };
    let plan = if tool == "TodoWrite" {
        input
            .and_then(|input| input.get("todos"))
            .and_then(Value::as_array)
            .map(|todos| {
                todos
                    .iter()
                    .take(MAX_PLAN_ITEMS)
                    .filter_map(plan_item)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let args = input
        .into_iter()
        .flatten()
        .filter(|(key, _)| !skip.contains(&key.as_str()))
        .filter_map(|(key, value)| Some((key.clone(), arg(value)?)))
        .take(MAX_ARGS)
        .collect();
    Some(Step {
        id: id.to_owned(),
        parent: parent.filter(|parent| valid_id(parent)).map(str::to_owned),
        kind,
        tool: tool.to_owned(),
        args,
        edit,
        plan,
    })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn kind(tool: &str) -> StepKind {
    match tool {
        "Bash" | "BashOutput" | "KillShell" => StepKind::Command,
        "Read" | "NotebookRead" => StepKind::Read,
        "Edit" | "MultiEdit" | "NotebookEdit" => StepKind::Edit,
        "Write" => StepKind::Write,
        "Grep" | "Glob" | "ToolSearch" => StepKind::Search,
        "WebFetch" | "WebSearch" => StepKind::Web,
        "TodoWrite" => StepKind::Plan,
        "Task" | "Agent" => StepKind::Subagent,
        "ReadMcpResourceTool" | "ListMcpResourcesTool" => StepKind::Mcp,
        _ if tool.starts_with("mcp__") => StepKind::Mcp,
        _ => StepKind::Other,
    }
}

/// An argument as text: a string as written, a list of strings one per line, and anything nested
/// as one `name: value` line per leaf. Strings keep their real newlines, never JSON escapes, so
/// the daemon's secret rules see the text as it was written.
fn arg(value: &Value) -> Option<String> {
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

fn plan_item(todo: &Value) -> Option<PlanItem> {
    let text = todo.get("content").and_then(Value::as_str)?;
    let status = match todo.get("status").and_then(Value::as_str) {
        Some("completed") => PlanStatus::Completed,
        Some("in_progress") => PlanStatus::InProgress,
        _ => PlanStatus::Pending,
    };
    Some(PlanItem {
        text: text.to_owned(),
        status,
    })
}

/// A result's size in bytes, its first characters, which carry a command's exit status, and its
/// text cut to what the daemon may read.
fn result_text(content: Option<&Value>) -> (u64, String, Option<String>) {
    let pieces: Vec<&str> = match content {
        Some(Value::String(text)) => vec![text.as_str()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    };
    let bytes = pieces.iter().map(|piece| piece.len() as u64).sum();
    let head = pieces
        .first()
        .map(|piece| piece.chars().take(24).collect())
        .unwrap_or_default();
    let text = (bytes > 0).then(|| crate::stream::bound_to(&pieces.join("\n"), MAX_RESULT_BYTES));
    (bytes, head, text)
}

fn exit_code(head: &str) -> Option<i32> {
    head.strip_prefix("Exit code ")?
        .split(|c: char| !(c.is_ascii_digit() || c == '-'))
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::parse_line;

    const SESSION: &str = "511d87b0-cfa9-4d55-bb92-47360e5b69d4";

    fn read(steps: &mut LiveSteps, text: &str) -> Vec<StepEvent> {
        let line = parse_line(text).expect("a stream line");
        steps.read(&line, Some(&SessionId::new(SESSION)))
    }

    /// A `Bash` call and its result from a captured two-turn run.
    fn captured() -> (&'static str, &'static str) {
        let fixture = include_str!("../../tests/fixtures/claude/r1-hooks.stdout");
        let mut lines = fixture
            .lines()
            .filter(|line| line.contains("toolu_8eSz2eVyn0jAPdqiJq59sxj7"));
        (lines.next().unwrap(), lines.next().unwrap())
    }

    #[test]
    fn a_captured_bash_call_opens_and_closes_with_its_result_size() {
        let (call, result) = captured();
        let mut steps = LiveSteps::default();
        let opened = read(&mut steps, call);
        let [StepEvent::Open { step, .. }] = &opened[..] else {
            panic!("one open step expected, got {opened:?}");
        };
        assert_eq!(step.id, "toolu_8eSz2eVyn0jAPdqiJq59sxj7");
        assert_eq!(step.kind, StepKind::Command);
        assert_eq!(step.tool, "Bash");
        assert_eq!(step.parent, None);
        assert!(
            step.args
                .contains(&("command".to_owned(), "echo hello-two".to_owned()))
        );
        let closed = read(&mut steps, result);
        assert_eq!(
            closed,
            [StepEvent::Close {
                session: SessionId::new(SESSION),
                id: "toolu_8eSz2eVyn0jAPdqiJq59sxj7".to_owned(),
                failed: false,
                exit: None,
                result_bytes: 9,
                result: Some("hello-two".to_owned()),
            }]
        );
    }

    #[test]
    fn a_result_for_a_call_never_opened_is_ignored() {
        let (_, result) = captured();
        assert!(read(&mut LiveSteps::default(), result).is_empty());
    }

    #[test]
    fn lines_of_another_session_open_nothing() {
        let (call, _) = captured();
        let line = parse_line(call).unwrap();
        let other = SessionId::new("other");
        assert!(LiveSteps::default().read(&line, Some(&other)).is_empty());
        assert!(LiveSteps::default().read(&line, None).is_empty());
    }

    #[test]
    fn a_failed_command_reports_its_exit_status() {
        let mut steps = LiveSteps::default();
        read(
            &mut steps,
            &format!(
                r#"{{"type":"assistant","session_id":"{SESSION}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_1","name":"Bash","input":{{"command":"false"}}}}]}}}}"#
            ),
        );
        let closed = read(
            &mut steps,
            &format!(
                r#"{{"type":"user","session_id":"{SESSION}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":"Exit code 3\nboom","is_error":true}}]}}}}"#
            ),
        );
        assert_eq!(
            closed,
            [StepEvent::Close {
                session: SessionId::new(SESSION),
                id: "toolu_1".to_owned(),
                failed: true,
                exit: Some(3),
                result_bytes: 16,
                result: Some("Exit code 3\nboom".to_owned()),
            }]
        );
    }

    #[test]
    fn a_result_of_text_blocks_is_joined_and_a_huge_one_is_cut_for_the_daemon() {
        let mut steps = LiveSteps::default();
        read(
            &mut steps,
            &format!(
                r#"{{"type":"assistant","session_id":"{SESSION}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_1","name":"Read","input":{{"file_path":"/a"}}}},{{"type":"tool_use","id":"toolu_2","name":"Read","input":{{"file_path":"/b"}}}}]}}}}"#
            ),
        );
        let huge = "h".repeat(MAX_RESULT_BYTES * 2);
        let closed = read(
            &mut steps,
            &format!(
                r#"{{"type":"user","session_id":"{SESSION}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":[{{"type":"text","text":"one"}},{{"type":"image"}},{{"type":"text","text":"two"}}]}},{{"type":"tool_result","tool_use_id":"toolu_2","content":"{huge}"}}]}}}}"#
            ),
        );
        let [
            StepEvent::Close {
                result: first,
                result_bytes: 6,
                ..
            },
            StepEvent::Close {
                result: Some(second),
                result_bytes,
                ..
            },
        ] = &closed[..]
        else {
            panic!("two closes expected, got {closed:?}");
        };
        assert_eq!(first.as_deref(), Some("one\ntwo"));
        assert_eq!(*result_bytes, huge.len() as u64);
        assert!(second.len() <= MAX_RESULT_BYTES && second.ends_with('…'));
    }

    #[test]
    fn a_helper_agents_calls_nest_under_the_call_that_started_it() {
        let mut steps = LiveSteps::default();
        let opened = read(
            &mut steps,
            &format!(
                r#"{{"type":"assistant","parent_tool_use_id":"toolu_task","session_id":"{SESSION}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_inner","name":"Read","input":{{"file_path":"/repo/a.rs"}}}}]}}}}"#
            ),
        );
        let [StepEvent::Open { step, .. }] = &opened[..] else {
            panic!("one open step expected");
        };
        assert_eq!(step.parent.as_deref(), Some("toolu_task"));
        assert_eq!(step.kind, StepKind::Read);
    }

    #[test]
    fn an_edit_carries_its_old_and_new_text_and_a_write_its_content() {
        let mut steps = LiveSteps::default();
        let events = read(
            &mut steps,
            &format!(
                r#"{{"type":"assistant","session_id":"{SESSION}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_e","name":"Edit","input":{{"file_path":"/repo/a.rs","old_string":"let a = 1;","new_string":"let a = 2;"}}}},{{"type":"tool_use","id":"toolu_w","name":"Write","input":{{"file_path":"/repo/b.rs","content":"fn b() {{}}"}}}}]}}}}"#
            ),
        );
        let [
            StepEvent::Open { step: edit, .. },
            StepEvent::Open { step: write, .. },
        ] = &events[..]
        else {
            panic!("two open steps expected");
        };
        assert_eq!(
            edit.edit,
            Some(EditText {
                old: Some("let a = 1;".to_owned()),
                new: "let a = 2;".to_owned()
            })
        );
        assert_eq!(
            edit.args,
            [("file_path".to_owned(), "/repo/a.rs".to_owned())]
        );
        assert_eq!(
            write.edit,
            Some(EditText {
                old: None,
                new: "fn b() {}".to_owned()
            })
        );
        assert_eq!(write.kind, StepKind::Write);
    }

    #[test]
    fn todo_write_is_a_plan_with_a_status_per_item() {
        let events = read(
            &mut LiveSteps::default(),
            &format!(
                r#"{{"type":"assistant","session_id":"{SESSION}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_p","name":"TodoWrite","input":{{"todos":[{{"content":"Write the reader","status":"completed","activeForm":"Writing"}},{{"content":"Test it","status":"in_progress","activeForm":"Testing"}},{{"content":"Ship","status":"pending","activeForm":"Shipping"}}]}}}}]}}}}"#
            ),
        );
        let [StepEvent::Open { step, .. }] = &events[..] else {
            panic!("one open step expected");
        };
        assert_eq!(step.kind, StepKind::Plan);
        assert!(step.args.is_empty());
        let plan: Vec<_> = step
            .plan
            .iter()
            .map(|item| (item.text.as_str(), item.status))
            .collect();
        assert_eq!(
            plan,
            [
                ("Write the reader", PlanStatus::Completed),
                ("Test it", PlanStatus::InProgress),
                ("Ship", PlanStatus::Pending),
            ]
        );
    }

    #[test]
    fn nested_arguments_keep_real_newlines_and_unescaped_strings() {
        let events = read(
            &mut LiveSteps::default(),
            &format!(
                r#"{{"type":"assistant","session_id":"{SESSION}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_m","name":"mcp__x__run","input":{{"opts":{{"path":"/w/.env","body":"first\nghp_abcdefghijklmnopqrstuvwxyz0123456789","password":"p \"q\""}},"n":3}}}}]}}}}"#
            ),
        );
        let [StepEvent::Open { step, .. }] = &events[..] else {
            panic!("one open step expected");
        };
        let opts = &step.args.iter().find(|(key, _)| key == "opts").unwrap().1;
        for line in [
            "path: /w/.env",
            "body: first\nghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "password: p \"q\"",
        ] {
            assert!(opts.contains(line), "{line} missing from {opts}");
        }
        assert!(!opts.contains("\\n"), "no JSON escapes: {opts}");
    }

    #[test]
    fn a_huge_nested_argument_is_cut_with_a_mark() {
        let numbers = vec!["1234567890"; 20_000].join(",");
        let events = read(
            &mut LiveSteps::default(),
            &format!(
                r#"{{"type":"assistant","session_id":"{SESSION}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_h","name":"mcp__x__run","input":{{"opts":{{"list":[{numbers}]}}}}}}]}}}}"#
            ),
        );
        let [StepEvent::Open { step, .. }] = &events[..] else {
            panic!("one open step expected");
        };
        let opts = &step.args[0].1;
        assert!(opts.len() <= MAX_ARG_BYTES, "{}", opts.len());
        assert!(opts.ends_with('…'));
    }

    #[test]
    fn a_second_call_with_an_open_id_is_not_a_step() {
        let call = format!(
            r#"{{"type":"assistant","session_id":"{SESSION}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_d","name":"Bash","input":{{}}}}]}}}}"#
        );
        let mut steps = LiveSteps::default();
        assert_eq!(read(&mut steps, &call).len(), 1);
        assert!(read(&mut steps, &call).is_empty());
    }

    #[test]
    fn a_tool_use_with_an_unusable_id_is_not_a_step() {
        let events = read(
            &mut LiveSteps::default(),
            &format!(
                r#"{{"type":"assistant","session_id":"{SESSION}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"has space","name":"Bash","input":{{}}}}]}}}}"#
            ),
        );
        assert!(events.is_empty());
    }
}
