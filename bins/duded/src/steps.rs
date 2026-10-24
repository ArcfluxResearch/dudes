//! A turn's steps, made safe to show. Every text field passes the daemon's secret scrub before it
//! is cut to size or kept, so nothing past this module holds a secret the daemon knows, and a
//! step that names a sensitive path or carries a secret-shaped value loses its edit text too.
//! Results are not here: a step carries only how it ended and how big its result was.

use std::time::{SystemTime, UNIX_EPOCH};

use dudes_providers::steps::{EditText, Step, StepKind, StepNote};
use dudes_supervisor::{REDACTED, Redactor};
use serde::Serialize;

use crate::secret_patterns::{contains_secret, sensitive_path};

/// The most an input, or one side of an edit, shows. The rest is cut and marked.
pub(crate) const INPUT_BYTES: usize = 2 * 1024;
const PLAN_ITEM_BYTES: usize = 512;
const NAME_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Running,
    Done,
    Failed,
}

#[derive(Clone, Serialize)]
pub(crate) struct ArgView {
    pub(crate) key: String,
    pub(crate) value: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) cut: bool,
}

#[derive(Clone, Serialize)]
pub(crate) struct EditView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) old: Option<String>,
    pub(crate) new: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) cut: bool,
}

#[derive(Clone, Serialize)]
pub(crate) struct PlanView {
    pub(crate) text: String,
    pub(crate) status: &'static str,
}

/// One step as the browser receives it.
#[derive(Clone, Serialize)]
pub(crate) struct StepView {
    pub(crate) id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent: Option<String>,
    pub(crate) kind: &'static str,
    pub(crate) tool: String,
    pub(crate) status: Status,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) args: Vec<ArgView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) edit: Option<EditView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) plan: Vec<PlanView>,
    /// Why the edit text, and any result, stay hidden.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) withheld: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) exit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) note: Option<&'static str>,
    pub(crate) started_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ended_at: Option<u64>,
}

impl std::fmt::Debug for StepView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StepView")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// How a step ended.
#[derive(Clone, Debug)]
pub(crate) struct Closing {
    pub(crate) id: String,
    pub(crate) failed: bool,
    pub(crate) exit: Option<i32>,
    pub(crate) result_bytes: u64,
    pub(crate) note: Option<StepNote>,
    pub(crate) at: u64,
}

impl StepView {
    pub(crate) fn close(&mut self, closing: &Closing) {
        self.status = if closing.failed {
            Status::Failed
        } else {
            Status::Done
        };
        self.exit = closing.exit;
        self.result_bytes = Some(closing.result_bytes);
        self.note = closing.note.map(StepNote::text);
        self.ended_at = Some(closing.at);
    }

    /// A step that was still open when its turn ended will not get a result.
    pub(crate) fn abandon(&mut self, at: u64) {
        if self.status == Status::Running {
            self.status = Status::Failed;
            self.note = Some("the turn ended first");
            self.ended_at = Some(at);
        }
    }

    /// What this step costs in memory, to the nearest few bytes.
    pub(crate) fn bytes(&self) -> usize {
        const BASE: usize = 128;
        BASE + self.id.len()
            + self.tool.len()
            + self.parent.as_ref().map_or(0, String::len)
            + self
                .args
                .iter()
                .map(|arg| arg.key.len() + arg.value.len())
                .sum::<usize>()
            + self.edit.as_ref().map_or(0, |edit| {
                edit.old.as_ref().map_or(0, String::len) + edit.new.len()
            })
            + self
                .plan
                .iter()
                .map(|item| item.text.len() + 16)
                .sum::<usize>()
    }
}

/// Unix milliseconds now.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The safe form of a step the engine just started.
pub(crate) fn open(redactor: &Redactor, step: Step, now: u64) -> StepView {
    let Step {
        id,
        parent,
        kind,
        tool,
        args,
        edit,
        plan,
    } = step;
    let mut withheld = None;
    let args: Vec<ArgView> = args
        .into_iter()
        .map(|(key, value)| {
            let value = redactor.scrub(&value);
            if names_sensitive_path(kind, &tool, &key, &value) {
                withheld = Some("the step names a sensitive path");
            }
            let value = if contains_secret(value.as_bytes()) {
                withheld.get_or_insert("the step holds a secret");
                REDACTED.to_owned()
            } else {
                value.into_owned()
            };
            let (value, cut) = cap(&value, INPUT_BYTES);
            ArgView {
                key: cap(&redactor.scrub(&key), NAME_BYTES).0,
                value,
                cut,
            }
        })
        .collect();
    let edit = edit.and_then(|edit| edit_view(redactor, edit, &mut withheld));
    let plan = plan
        .into_iter()
        .map(|item| {
            let text = redactor.scrub(&item.text);
            let text = if contains_secret(text.as_bytes()) {
                REDACTED
            } else {
                &text
            };
            PlanView {
                text: cap(text, PLAN_ITEM_BYTES).0,
                status: item.status.name(),
            }
        })
        .collect();
    StepView {
        id,
        parent,
        kind: kind.name(),
        tool: cap(&redactor.scrub(&tool), NAME_BYTES).0,
        status: Status::Running,
        args,
        edit,
        plan,
        withheld,
        exit: None,
        result_bytes: None,
        note: None,
        started_at: now,
        ended_at: None,
    }
}

/// The edit text, unless the file is sensitive or the text holds a secret. The scrub runs on the
/// whole text and the cut after it, so a secret on the cut line is never half kept.
fn edit_view(
    redactor: &Redactor,
    edit: EditText,
    withheld: &mut Option<&'static str>,
) -> Option<EditView> {
    let EditText { old, new } = edit;
    let old = old.map(|old| redactor.scrub(&old).into_owned());
    let new = redactor.scrub(&new).into_owned();
    if old
        .as_deref()
        .is_some_and(|old| contains_secret(old.as_bytes()))
        || contains_secret(new.as_bytes())
    {
        withheld.get_or_insert("the text holds a secret");
    }
    if withheld.is_some() {
        return None;
    }
    let (old, old_cut) = match old {
        Some(old) => {
            let (old, cut) = cap(&old, INPUT_BYTES);
            (Some(old), cut)
        }
        None => (None, false),
    };
    let (new, new_cut) = cap(&new, INPUT_BYTES);
    Some(EditView {
        old,
        new,
        cut: old_cut || new_cut,
    })
}

/// Whether an argument names a file the Changes view would refuse to show. A key that holds a
/// path, and a value that looks like one, are checked line by line, and so is each word of a
/// command.
fn names_sensitive_path(kind: StepKind, tool: &str, key: &str, value: &str) -> bool {
    let key_is_path = matches!(
        key,
        "path" | "file_path" | "paths" | "directory" | "cwd" | "notebook_path"
    ) || (tool == "Glob" && key == "pattern");
    let command = kind == StepKind::Command && key == "command";
    value.lines().any(|line| {
        let line = line.trim();
        if command {
            line.split(|c: char| {
                c.is_whitespace()
                    || matches!(
                        c,
                        ';' | '|' | '&' | '<' | '>' | '(' | ')' | '"' | '\'' | '`' | '='
                    )
            })
            .filter(|word| !word.is_empty())
            .any(names_sensitive)
        } else {
            (key_is_path || path_like(line)) && names_sensitive(line)
        }
    })
}

/// A file the Changes view refuses, or a directory that holds such files: a search over
/// `~/.ssh` reads them as surely as a read of one does.
fn names_sensitive(path: &str) -> bool {
    sensitive_path(path) || sensitive_path(&format!("{}/.", path.trim_end_matches('/')))
}

fn path_like(value: &str) -> bool {
    value.starts_with(['/', '~', '.']) || value.contains('/')
}

/// `text` cut to `limit` bytes on a character boundary, and whether it was cut. A cut ends in an
/// ellipsis the Floor follows with a plain word.
fn cap(text: &str, limit: usize) -> (String, bool) {
    (
        dudes_providers::stream::bound_to(text, limit),
        text.len() > limit,
    )
}

#[cfg(test)]
mod tests {
    use dudes_providers::steps::{PlanItem, PlanStatus};

    use super::*;

    const SECRET: &str = "synthetic-secret-value-0123456789";

    fn redactor() -> Redactor {
        let redactor = Redactor::for_run();
        redactor.set_source("test", [SECRET.to_owned()]);
        redactor
    }

    fn step(kind: StepKind, tool: &str, args: &[(&str, &str)]) -> Step {
        Step {
            id: "toolu_1".to_owned(),
            parent: None,
            kind,
            tool: tool.to_owned(),
            args: args
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            edit: None,
            plan: Vec::new(),
        }
    }

    fn json(view: &StepView) -> String {
        serde_json::to_string(view).unwrap()
    }

    #[test]
    fn a_known_secret_in_a_command_never_reaches_the_wire() {
        let command = format!("curl -H 'Authorization: Bearer {SECRET}' https://example.test");
        let view = open(
            &redactor(),
            step(StepKind::Command, "Bash", &[("command", &command)]),
            1,
        );
        let wire = json(&view);
        assert!(!wire.contains(SECRET), "{wire}");
        assert!(wire.contains("(redacted)"), "{wire}");
    }

    #[test]
    fn a_secret_in_an_edit_and_in_a_path_argument_is_scrubbed() {
        let mut edit = step(
            StepKind::Edit,
            "Edit",
            &[("file_path", &format!("/repo/{SECRET}.rs"))],
        );
        edit.edit = Some(EditText {
            old: Some(format!("let key = \"{SECRET}\";")),
            new: format!("let key = env!(\"{SECRET}\");"),
        });
        let wire = json(&open(&redactor(), edit, 1));
        assert!(!wire.contains(SECRET), "{wire}");
        assert!(wire.contains("(redacted)"), "{wire}");
    }

    #[test]
    fn a_step_naming_a_sensitive_path_shows_its_input_and_hides_its_edit() {
        for (kind, tool, args) in [
            (
                StepKind::Command,
                "Bash",
                ("command", "cat ~/.aws/credentials"),
            ),
            (StepKind::Read, "Read", ("file_path", "/home/me/.env")),
            (StepKind::Search, "Grep", ("path", "~/.ssh")),
            (
                StepKind::Mcp,
                "mcp__files__read",
                ("path", "/home/me/.aws/config"),
            ),
            (StepKind::Search, "Glob", ("pattern", "/home/me/.ssh/id_*")),
        ] {
            let mut sensitive = step(kind, tool, &[args]);
            sensitive.edit = Some(EditText {
                old: None,
                new: "plain text".to_owned(),
            });
            let view = open(&redactor(), sensitive, 1);
            assert!(view.withheld.is_some(), "{tool} {args:?}");
            assert!(view.edit.is_none(), "{tool} {args:?}");
            assert_eq!(view.args[0].value, args.1, "the input stays visible");
        }
    }

    #[test]
    fn an_ordinary_edit_keeps_its_old_and_new_text() {
        let mut edit = step(StepKind::Edit, "Edit", &[("file_path", "/repo/src/a.rs")]);
        edit.edit = Some(EditText {
            old: Some("let a = 1;".to_owned()),
            new: "let a = 2;".to_owned(),
        });
        let view = open(&redactor(), edit, 1);
        assert_eq!(view.withheld, None);
        let kept = view.edit.expect("the edit text");
        assert_eq!(kept.old.as_deref(), Some("let a = 1;"));
        assert_eq!(kept.new, "let a = 2;");
    }

    #[test]
    fn a_secret_shaped_value_is_replaced_and_hides_the_edit() {
        let mut write = step(
            StepKind::Write,
            "Write",
            &[("file_path", "/repo/notes.txt")],
        );
        write.edit = Some(EditText {
            old: None,
            new: "token ghp_abcdefghijklmnopqrstuvwxyz0123456789".to_owned(),
        });
        let view = open(&redactor(), write, 1);
        assert!(view.edit.is_none());
        assert_eq!(view.withheld, Some("the text holds a secret"));
        let command = step(
            StepKind::Command,
            "Bash",
            &[(
                "command",
                "export K=ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            )],
        );
        let view = open(&redactor(), command, 1);
        assert_eq!(view.args[0].value, "(redacted)");
    }

    #[test]
    fn a_long_input_is_cut_with_a_mark_and_a_short_one_is_not() {
        let long = "x".repeat(INPUT_BYTES + 500);
        let view = open(
            &Redactor::for_run(),
            step(
                StepKind::Command,
                "Bash",
                &[("command", &long), ("description", "short")],
            ),
            1,
        );
        assert!(view.args[0].cut && view.args[0].value.ends_with('…'));
        assert!(view.args[0].value.len() <= INPUT_BYTES);
        assert!(!view.args[1].cut);
    }

    #[test]
    fn a_secret_that_sits_on_the_cut_line_is_scrubbed_before_the_cut() {
        let text = format!("{}{SECRET}", "x".repeat(INPUT_BYTES - 10));
        let view = open(
            &redactor(),
            step(StepKind::Command, "Bash", &[("command", &text)]),
            1,
        );
        assert!(!json(&view).contains("synthetic-secret"));
    }

    #[test]
    fn a_plan_keeps_its_items_and_scrubs_them() {
        let mut plan = step(StepKind::Plan, "TodoWrite", &[]);
        plan.plan = vec![
            PlanItem {
                text: format!("Rotate {SECRET}"),
                status: PlanStatus::InProgress,
            },
            PlanItem {
                text: "Ship".to_owned(),
                status: PlanStatus::Pending,
            },
        ];
        let view = open(&redactor(), plan, 1);
        assert_eq!(view.plan[0].text, "Rotate (redacted)");
        assert_eq!(view.plan[0].status, "in_progress");
        assert_eq!(view.plan[1].text, "Ship");
    }

    #[test]
    fn closing_and_abandoning_a_step() {
        let mut view = open(
            &Redactor::for_run(),
            step(StepKind::Command, "Bash", &[]),
            10,
        );
        view.close(&Closing {
            id: "toolu_1".to_owned(),
            failed: true,
            exit: Some(2),
            result_bytes: 40,
            note: None,
            at: 20,
        });
        assert_eq!(view.status, Status::Failed);
        assert_eq!(
            (view.exit, view.result_bytes, view.ended_at),
            (Some(2), Some(40), Some(20))
        );
        let mut open_one = open(&Redactor::for_run(), step(StepKind::Read, "Read", &[]), 10);
        open_one.abandon(30);
        assert_eq!(open_one.note, Some("the turn ended first"));
        view.abandon(99);
        assert_eq!(view.ended_at, Some(20), "a closed step stays as it ended");
    }
}
