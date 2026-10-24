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
