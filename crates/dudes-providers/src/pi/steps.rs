//! Pi's tool calls as steps.
//!
//! A call opens on `tool_execution_start` and closes on `tool_execution_end`. Pi's tool names
//! map to kinds, and an unknown name is `other`:
//!
//! | Tool                          | Kind    | Input                                             |
//! |-------------------------------|---------|---------------------------------------------------|
//! | `bash`                        | command | `command`                                         |
//! | `read`                        | read    | `file_path`, `offset`, `limit`                    |
//! | `edit`                        | edit    | `file_path`, and the text of a lone replacement   |
//! | `write`                       | write   | `file_path`, and the file's content as the edit   |
//! | `grep`, `find`, `ls`          | search  | `pattern`, `path`, and the rest as sent           |
//! | `mcp__<server>__<tool>`       | mcp     | the arguments as sent                             |
//!
//! A result is the text of the result's content. Pi reports failure with `isError` and no exit
//! status, and no call has a parent.

use std::collections::BTreeSet;

use dudes_types::SessionId;
use serde_json::Value;

use crate::steps::{
    EditText, MAX_OPEN, Step, StepEvent, StepKind, bounded_result, object_args, valid_id,
};

#[derive(Debug, Default)]
pub(super) struct LiveSteps {
    open: BTreeSet<String>,
}

impl LiveSteps {
    pub(super) fn start(
        &mut self,
        session: &SessionId,
        id: &str,
        name: &str,
        args: Option<&Value>,
    ) -> Option<StepEvent> {
        if !valid_id(id)
            || name.is_empty()
            || self.open.len() >= MAX_OPEN
            || !self.open.insert(id.to_owned())
        {
            return None;
        }
        Some(StepEvent::Open {
            session: session.clone(),
            step: step(id, name, args),
        })
    }

    pub(super) fn end(
        &mut self,
        session: &SessionId,
        id: &str,
        failed: bool,
        result: &str,
    ) -> Option<StepEvent> {
        self.open.remove(id).then(|| {
            let (result_bytes, result) = bounded_result(result);
            StepEvent::Close {
                session: session.clone(),
                id: id.to_owned(),
                failed,
                exit: None,
                result_bytes,
                result,
            }
        })
    }
}

fn kind(tool: &str) -> StepKind {
    match tool {
        "bash" => StepKind::Command,
        "read" => StepKind::Read,
        "edit" => StepKind::Edit,
        "write" => StepKind::Write,
        "grep" | "find" | "ls" => StepKind::Search,
        "list_mcp_resources" | "list_mcp_resource_templates" | "read_mcp_resource" => StepKind::Mcp,
        _ if tool.starts_with("mcp__") => StepKind::Mcp,
        _ => StepKind::Other,
    }
}

fn step(id: &str, tool: &str, args: Option<&Value>) -> Step {
    let kind = kind(tool);
    let input = args.and_then(Value::as_object);
    let text = |key: &str| {
        input
            .and_then(|input| input.get(key))
            .and_then(Value::as_str)
    };
    // A path is a `file_path` for a file's tool, the key the daemon and the Floor read for one.
    let file_tool = matches!(kind, StepKind::Read | StepKind::Edit | StepKind::Write);
    let (edit, skip): (_, &[&str]) = match tool {
        "edit" => {
            let edits = input
                .and_then(|input| input.get("edits"))
                .and_then(Value::as_array);
            let lone = match edits.map(Vec::as_slice) {
                Some([only]) => only.as_object(),
                _ => None,
            };
            let new = lone
                .and_then(|edit| edit.get("newText"))
                .and_then(Value::as_str);
            (
                new.map(|new| EditText {
                    old: lone
                        .and_then(|edit| edit.get("oldText"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    new: new.to_owned(),
                }),
                if new.is_some() { &["edits"] } else { &[] },
            )
        }
        "write" => (
            text("content").map(|new| EditText {
                old: None,
                new: new.to_owned(),
            }),
            &["content"],
        ),
        _ => (None, &[]),
    };
    let mut args = object_args(input, skip);
    if file_tool {
        for (key, _) in &mut args {
            if key == "path" {
                "file_path".clone_into(key);
            }
        }
    }
    Step {
        id: id.to_owned(),
        parent: None,
        kind,
        tool: tool.to_owned(),
        args,
        edit,
        plan: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pi::stream::TurnStream;
    use crate::steps::MAX_RESULT_BYTES;
    use crate::stream::TurnReader;

    const SESSION: &str = "01a0e1e4-ff40-736b-8ce4-31fdba7fd3a2";

    fn run(lines: &[String]) -> Vec<StepEvent> {
        let mut stream = TurnStream::new();
        stream.line(&format!(
            r#"{{"type":"session","version":3,"id":"{SESSION}"}}"#
        ));
        lines
            .iter()
            .flat_map(|line| stream.line(line).steps)
            .collect()
    }

    fn start(id: &str, tool: &str, args: &str) -> String {
        format!(
            r#"{{"type":"tool_execution_start","toolCallId":"{id}","toolName":"{tool}","args":{args}}}"#
        )
    }

    fn end(id: &str, tool: &str, text: &str, error: bool) -> String {
        format!(
            r#"{{"type":"tool_execution_end","toolCallId":"{id}","toolName":"{tool}","result":{{"content":[{{"type":"text","text":"{text}"}}]}},"isError":{error}}}"#
        )
    }

    fn open(event: &StepEvent) -> &Step {
        let StepEvent::Open { step, .. } = event else {
            panic!("an open step expected, got {event:?}");
        };
        step
    }

    #[test]
    fn a_captured_read_opens_and_closes_with_its_text() {
        let fixture = include_str!("../../tests/fixtures/pi/v1-l2-tool.jsonl");
        let mut stream = TurnStream::new();
        let events: Vec<StepEvent> = fixture
            .lines()
            .flat_map(|line| stream.line(line).steps)
            .collect();
        let [opened, closed] = &events[..] else {
            panic!("an open and a close expected, got {events:?}");
        };
        let step = open(opened);
        assert_eq!(
            (step.id.as_str(), step.kind, step.tool.as_str()),
            ("call_probe_1", StepKind::Read, "read")
        );
        assert_eq!(step.args, [("file_path".to_owned(), "note.txt".to_owned())]);
        assert_eq!(
            closed,
            &StepEvent::Close {
                session: SessionId::new("b8917449-c647-7358-bf7e-68e9dee2fae7"),
                id: "call_probe_1".to_owned(),
                failed: false,
                exit: None,
                result_bytes: 23,
                result: Some("The code word is PLUM.\n".to_owned()),
            }
        );
    }

    #[test]
    fn each_tool_maps_to_its_kind_and_an_unknown_one_is_other() {
        let tools = [
            ("bash", r#"{"command":"ls"}"#, StepKind::Command),
            (
                "edit",
                r#"{"path":"a.rs","edits":[{"oldText":"a","newText":"b"}]}"#,
                StepKind::Edit,
            ),
            (
                "write",
                r#"{"path":"b.rs","content":"fn b() {}"}"#,
                StepKind::Write,
            ),
            (
                "grep",
                r#"{"pattern":"todo","path":"src"}"#,
                StepKind::Search,
            ),
            ("find", r#"{"pattern":"*.rs"}"#, StepKind::Search),
            ("ls", r#"{"path":"."}"#, StepKind::Search),
            ("mcp__docs__find", r"{}", StepKind::Mcp),
            ("teleport", r"{}", StepKind::Other),
        ];
        let lines: Vec<String> = tools
            .iter()
            .enumerate()
            .map(|(index, (tool, args, _))| start(&format!("c{index}"), tool, args))
            .collect();
        let events = run(&lines);
        let kinds: Vec<_> = events.iter().map(|event| open(event).kind).collect();
        assert_eq!(kinds, tools.map(|(_, _, kind)| kind));
    }

    #[test]
    fn an_edit_and_a_write_carry_their_text() {
        let events = run(&[
            start(
                "e",
                "edit",
                r#"{"path":"a.rs","edits":[{"oldText":"let a = 1;","newText":"let a = 2;"}]}"#,
            ),
            start("w", "write", r#"{"path":"b.rs","content":"fn b() {}"}"#),
        ]);
        let (edit, write) = (open(&events[0]), open(&events[1]));
        assert_eq!(
            edit.edit,
            Some(EditText {
                old: Some("let a = 1;".to_owned()),
                new: "let a = 2;".to_owned()
            })
        );
        assert_eq!(edit.args, [("file_path".to_owned(), "a.rs".to_owned())]);
        assert_eq!(
            write.edit,
            Some(EditText {
                old: None,
                new: "fn b() {}".to_owned()
            })
        );
        assert_eq!(write.args, [("file_path".to_owned(), "b.rs".to_owned())]);
    }

    #[test]
    fn a_failed_call_closes_failed_and_a_long_result_is_cut() {
        let long = "x".repeat(MAX_RESULT_BYTES * 2);
        let events = run(&[
            start("a", "bash", r#"{"command":"false"}"#),
            end("a", "bash", "exit 1", true),
            start("b", "bash", r#"{"command":"yes"}"#),
            end("b", "bash", &long, false),
        ]);
        let StepEvent::Close { failed, result, .. } = &events[1] else {
            panic!("a close expected");
        };
        assert!(*failed);
        assert_eq!(result.as_deref(), Some("exit 1"));
        let StepEvent::Close {
            result_bytes,
            result: Some(cut),
            ..
        } = &events[3]
        else {
            panic!("a close expected");
        };
        assert_eq!(*result_bytes, long.len() as u64);
        assert!(cut.len() <= MAX_RESULT_BYTES && cut.ends_with('…'));
    }

    #[test]
    fn an_end_with_no_start_and_a_repeated_start_are_not_steps() {
        let events = run(&[
            end("ghost", "read", "x", false),
            start("dup", "read", r#"{"path":"a"}"#),
            start("dup", "read", r#"{"path":"b"}"#),
        ]);
        assert_eq!(events.len(), 1);
        assert_eq!(
            open(&events[0]).args,
            [("file_path".to_owned(), "a".to_owned())]
        );
    }
}
