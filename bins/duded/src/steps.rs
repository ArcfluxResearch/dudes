//! A turn's steps, made safe to show. Every text field passes the daemon's secret scrub before it
//! is cut to size or kept, so nothing past this module holds a secret the daemon knows, and a
//! step that names a sensitive path or carries a secret-shaped value loses its edit text too.
//! A step on the wire carries only how it ended and how big its result was. A result's text, and
//! an edit's text past the input cap, are kept apart (see `step_results`) and sent only when
//! the selected agent's page asks for them.

use std::time::{SystemTime, UNIX_EPOCH};

use dudes_providers::steps::{EditText, Step, StepKind};
use dudes_supervisor::{REDACTED, Redactor};
use serde::Serialize;

use crate::secret_patterns::{contains_secret, sensitive_path};

/// The most an input, or one side of an edit, shows. The rest is cut and marked.
pub(crate) const INPUT_BYTES: usize = 2 * 1024;
/// The most a kept result, or one side of a kept edit, holds. The rest is cut and marked.
pub(crate) const KEPT_BYTES: usize = 8 * 1024;
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
    pub(crate) result: Option<KeptResult>,
    pub(crate) at: u64,
}

/// Text a step keeps for a fetch: scrubbed when it arrived and cut to size.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct KeptText {
    pub(crate) text: String,
    pub(crate) cut: bool,
}

impl std::fmt::Debug for KeptText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeptText")
            .field("bytes", &self.text.len())
            .finish_non_exhaustive()
    }
}

/// A result as kept. One that held a secret-shaped value keeps nothing but that fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KeptResult {
    Text(KeptText),
    Secret,
}

/// An edit's text as kept, for an edit too long to show in the step itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeptEdit {
    pub(crate) old: Option<KeptText>,
    pub(crate) new: KeptText,
}

impl KeptText {
    pub(crate) const fn bytes(&self) -> usize {
        self.text.len()
    }
}

impl KeptResult {
    pub(crate) const fn bytes(&self) -> usize {
        match self {
            Self::Text(text) => text.bytes(),
            Self::Secret => 0,
        }
    }
}

impl KeptEdit {
    pub(crate) fn bytes(&self) -> usize {
        self.old.as_ref().map_or(0, KeptText::bytes) + self.new.bytes()
    }
}

/// Whether a finished step's result text is worth keeping for a fetch. A step that is withheld,
/// a connected tool's, and a step whose text is its own edit or plan never are.
pub(crate) fn keeps_result(view: &StepView) -> bool {
    view.withheld.is_none() && !matches!(view.kind, "mcp" | "plan" | "edit" | "write")
}

/// Scrubs a result's whole text, looks for a secret-shaped value in all of it, and only then cuts
/// it, so a value on the cut line is never half kept.
pub(crate) fn keep_result(redactor: &Redactor, text: &str) -> KeptResult {
    let text = redactor.scrub(text);
    if result_holds_secret(&text) {
        KeptResult::Secret
    } else {
        KeptResult::Text(keep_text(&text))
    }
}

/// Whether a result holds a secret-shaped value, in its text or in any `name: value` line.
pub(crate) fn result_holds_secret(text: &str) -> bool {
    holds_secret("result", text)
}

fn keep_text(text: &str) -> KeptText {
    let (text, cut) = cap(text, KEPT_BYTES);
    KeptText { text, cut }
}

/// A text with the secrets the daemon knows now taken out, for one learned after it was kept.
pub(crate) fn rescrub(redactor: &Redactor, text: &mut String) {
    if let std::borrow::Cow::Owned(clean) = redactor.scrub(text) {
        *text = clean;
    }
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
#[cfg(test)]
pub(crate) fn open(redactor: &Redactor, step: Step, now: u64) -> StepView {
    open_with_text(redactor, step, now).0
}

/// The safe form of a step the engine just started, and its edit text when that is too long to
/// show in the step and nothing about the step is withheld.
pub(crate) fn open_with_text(
    redactor: &Redactor,
    step: Step,
    now: u64,
) -> (StepView, Option<KeptEdit>) {
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
    let mut names_path = false;
    let tool_name = safe_name(redactor, &tool, &mut withheld);
    let mut views = Vec::with_capacity(args.len());
    for (key, value) in args {
        let key_name = safe_name(redactor, &key, &mut withheld);
        let value = redactor.scrub(&value);
        if names_sensitive_path(kind, &tool, &key, &value) {
            withheld = Some("the step names a sensitive path");
            names_path = true;
        }
        let value = if holds_secret(&key, &value) {
            withheld.get_or_insert("the step holds a secret");
            REDACTED.to_owned()
        } else {
            value.into_owned()
        };
        let (value, cut) = cap(&value, INPUT_BYTES);
        views.push((
            is_path_key(&tool, &key),
            ArgView {
                key: key_name,
                value,
                cut,
            },
        ));
    }
    let edit = edit.map(|EditText { old, new }| EditText {
        old: old.map(|old| redactor.scrub(&old).into_owned()),
        new: redactor.scrub(&new).into_owned(),
    });
    let shown = edit
        .as_ref()
        .and_then(|edit| edit_view(edit, &mut withheld));
    let kept = shown
        .as_ref()
        .filter(|shown| shown.cut)
        .zip(edit.as_ref())
        .map(|(_, edit)| KeptEdit {
            old: edit.old.as_deref().map(keep_text),
            new: keep_text(&edit.new),
        });
    // A tool can carry its text in arguments the reader does not lift out (a list of edits, a
    // notebook cell, an MCP write's content), so a withheld edit step, and any step that names a
    // sensitive path, shows its path arguments and nothing else. A command is its own input and
    // stays whole.
    let hide_text = match kind {
        StepKind::Command => false,
        StepKind::Edit | StepKind::Write => withheld.is_some(),
        _ => names_path,
    };
    let args = views
        .into_iter()
        .filter(|(path, _)| *path || !hide_text)
        .map(|(_, view)| view)
        .collect();
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
    let view = StepView {
        id: scrub_id(redactor, &id),
        parent: parent.map(|parent| scrub_id(redactor, &parent)),
        kind: kind.name(),
        tool: tool_name,
        status: Status::Running,
        args,
        edit: shown,
        plan,
        withheld,
        exit: None,
        result_bytes: None,
        note: None,
        started_at: now,
        ended_at: None,
    };
    (view, kept)
}

/// Whether an argument holds a secret: its text on its own, and each line as the `"key":"value"`
/// pair the credential rule reads. A nested leaf reads `name: value` and a top-level argument is
/// its value alone, so the key has to be put back in front of the value.
fn holds_secret(key: &str, value: &str) -> bool {
    let pair = |key: &str, text: &str| {
        contains_secret(format!("\"{key}\":\"{}\"", text.replace('"', "'")).as_bytes())
    };
    contains_secret(value.as_bytes())
        || value.lines().any(|line| {
            match line
                .split_once(": ")
                .filter(|(name, _)| !name.contains(' '))
            {
                Some((name, text)) => pair(leaf_name(name), text),
                None => pair(key, line),
            }
        })
}

/// The last name on a flattened path: `opts.paths[0]` is `paths`.
fn leaf_name(path: &str) -> &str {
    let mut name = path;
    while let Some(open) = name.strip_suffix(']').and_then(|n| n.rfind('[')) {
        name = &name[..open];
    }
    name.rsplit('.').next().unwrap_or(name)
}

/// A tool or argument name, cut to size, or the redaction mark when it holds a secret.
fn safe_name(redactor: &Redactor, text: &str, withheld: &mut Option<&'static str>) -> String {
    let text = redactor.scrub(text);
    if contains_secret(text.as_bytes()) {
        withheld.get_or_insert("the step holds a secret");
        REDACTED.to_owned()
    } else {
        cap(&text, NAME_BYTES).0
    }
}

/// A call id with any secret the daemon knows taken out. The same rule runs on the id of the
/// result that closes the call, so the two still match.
pub(crate) fn scrub_id(redactor: &Redactor, id: &str) -> String {
    redactor.scrub(id).into_owned()
}

impl StepView {
    /// This step with the secrets the daemon knows now taken out of every text it holds, for one
    /// learned after the step was kept.
    pub(crate) fn rescrubbed(mut self, redactor: &Redactor) -> Self {
        let scrub = |text: &mut String| rescrub(redactor, text);
        scrub(&mut self.id);
        scrub(&mut self.tool);
        if let Some(parent) = &mut self.parent {
            scrub(parent);
        }
        for arg in &mut self.args {
            scrub(&mut arg.key);
            scrub(&mut arg.value);
        }
        if let Some(edit) = &mut self.edit {
            if let Some(old) = &mut edit.old {
                scrub(old);
            }
            scrub(&mut edit.new);
        }
        for item in &mut self.plan {
            scrub(&mut item.text);
        }
        self
    }
}

/// The edit text, unless the file is sensitive or the text holds a secret. The text arrives
/// scrubbed whole and the cut comes after it, so a secret on the cut line is never half kept.
fn edit_view(edit: &EditText, withheld: &mut Option<&'static str>) -> Option<EditView> {
    let EditText { old, new } = edit;
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
            let (old, cut) = cap(old, INPUT_BYTES);
            (Some(old), cut)
        }
        None => (None, false),
    };
    let (new, new_cut) = cap(new, INPUT_BYTES);
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
    let key_is_path = is_path_key(tool, key);
    let command = kind == StepKind::Command && key == "command";
    value.lines().any(|line| {
        let line = line.trim();
        // A nested leaf reads `name: value`, and the value alone may be the path.
        let leaf = line
            .split_once(": ")
            .filter(|(name, _)| !name.contains(' '));
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
                || leaf.is_some_and(|(name, text)| {
                    let name = leaf_name(name);
                    (is_path_key(tool, name) || path_like(text)) && names_sensitive(text.trim())
                })
        }
    })
}

fn is_path_key(tool: &str, key: &str) -> bool {
    matches!(
        key,
        "path" | "file_path" | "paths" | "directory" | "cwd" | "notebook_path"
    ) || (tool == "Glob" && key == "pattern")
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
        let redactor = Redactor::new();
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
            (StepKind::Read, "Read", ("file_path", "/work/me/.env")),
            (StepKind::Search, "Grep", ("path", "~/.ssh")),
            (
                StepKind::Mcp,
                "mcp__files__read",
                ("path", "/work/me/.aws/config"),
            ),
            (StepKind::Search, "Glob", ("pattern", "/work/me/.ssh/id_*")),
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
            &Redactor::new(),
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
    fn a_token_after_a_newline_in_a_nested_argument_is_redacted() {
        let nested = "opts.note: first\nghp_abcdefghijklmnopqrstuvwxyz0123456789\nopts.password: \"hunter2-synthetic\"";
        let view = open(
            &redactor(),
            step(StepKind::Mcp, "mcp__x__run", &[("opts", nested)]),
            1,
        );
        let wire = json(&view);
        assert!(!wire.contains("ghp_"), "{wire}");
        assert!(!wire.contains("hunter2"), "{wire}");
        assert_eq!(view.withheld, Some("the step holds a secret"));
    }

    #[test]
    fn a_nested_sensitive_path_withholds_the_step() {
        for nested in [
            "opts.path: /w/.env",
            "opts.file_path: .env",
            "edits[0].path: ~/.aws/config",
        ] {
            let view = open(
                &redactor(),
                step(StepKind::Mcp, "mcp__x__read", &[("opts", nested)]),
                1,
            );
            assert!(view.withheld.is_some(), "{nested}");
        }
        let plain = open(
            &redactor(),
            step(
                StepKind::Mcp,
                "mcp__x__read",
                &[("opts", "opts.path: /w/src/a.rs")],
            ),
            1,
        );
        assert_eq!(plain.withheld, None);
    }

    #[test]
    fn a_multi_line_known_secret_inside_a_nested_string_is_scrubbed() {
        let multi = "line-one-synthetic-secret\nline-two-synthetic-secret";
        let redactor = Redactor::new();
        redactor.set_source("test", [multi.to_owned()]);
        let nested = format!("opts.body: before {multi} after");
        let wire = json(&open(
            &redactor,
            step(StepKind::Mcp, "mcp__x__run", &[("opts", &nested)]),
            1,
        ));
        assert!(!wire.contains("synthetic-secret"), "{wire}");
        assert!(wire.contains("before (redacted) after"), "{wire}");
    }

    #[test]
    fn a_withheld_edit_tool_shows_its_path_and_none_of_its_text() {
        for (tool, path_key, text_key, path) in [
            (
                "MultiEdit",
                "file_path",
                "edits[0].new_string",
                "/work/me/.env",
            ),
            (
                "MultiEdit",
                "file_path",
                "edits[0].new_string",
                "~/.aws/config",
            ),
            (
                "NotebookEdit",
                "notebook_path",
                "new_source",
                "/work/me/.env",
            ),
            (
                "NotebookEdit",
                "notebook_path",
                "new_source",
                "~/.aws/config",
            ),
        ] {
            let view = open(
                &redactor(),
                step(
                    StepKind::Edit,
                    tool,
                    &[(path_key, path), (text_key, "plain text of the edit")],
                ),
                1,
            );
            let wire = json(&view);
            assert!(view.withheld.is_some(), "{tool} {path}");
            assert!(!wire.contains("plain text of the edit"), "{wire}");
            assert_eq!(view.args.len(), 1, "{wire}");
            assert_eq!(view.args[0].value, path);
        }
        let ordinary = open(
            &redactor(),
            step(
                StepKind::Edit,
                "MultiEdit",
                &[("file_path", "/repo/a.rs"), ("edits[0].new_string", "kept")],
            ),
            1,
        );
        assert_eq!(
            ordinary.args.len(),
            2,
            "an ordinary edit keeps its arguments"
        );
    }

    #[test]
    fn a_secret_in_a_tool_name_or_argument_key_never_reaches_the_wire() {
        let key = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let view = open(
            &redactor(),
            step(StepKind::Mcp, key, &[(key, "value"), (SECRET, "value")]),
            1,
        );
        let wire = json(&view);
        assert!(!wire.contains("ghp_") && !wire.contains(SECRET), "{wire}");
        assert_eq!(view.tool, "(redacted)");
        assert_eq!(view.withheld, Some("the step holds a secret"));
    }

    #[test]
    fn a_secret_learned_after_capture_is_scrubbed_when_the_step_is_read() {
        let redactor = Redactor::new();
        let mut kept = step(
            StepKind::Command,
            "Bash",
            &[("command", "echo later-secret-0123456789")],
        );
        kept.edit = Some(EditText {
            old: None,
            new: "later-secret-0123456789".to_owned(),
        });
        let view = open(&redactor, kept, 1);
        redactor.set_source("test", ["later-secret-0123456789".to_owned()]);
        let wire = json(&view.rescrubbed(&redactor));
        assert!(!wire.contains("later-secret"), "{wire}");
    }

    const SESSION: &str = "511d87b0-cfa9-4d55-bb92-47360e5b69d4";

    /// A tool call as the Claude reader sees it on the wire, opened the way the daemon opens it.
    fn from_stream(tool: &str, input: &str) -> StepView {
        use dudes_providers::claude::stream::TurnStream;
        use dudes_providers::stream::TurnReader;
        let mut stream = TurnStream::new();
        stream.line(&format!(
            r#"{{"type":"system","subtype":"init","session_id":"{SESSION}","tools":[]}}"#
        ));
        let call = format!(
            r#"{{"type":"assistant","session_id":"{SESSION}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_1","name":"{tool}","input":{input}}}]}}}}"#
        );
        let mut events = stream.line(&call).steps;
        let Some(dudes_providers::steps::StepEvent::Open { step, .. }) = events.pop() else {
            panic!("one open step expected");
        };
        open(&Redactor::new(), step, 1)
    }

    #[test]
    fn a_credential_key_in_raw_tool_input_never_reaches_the_wire() {
        for key in ["password", "api_key", "client_secret"] {
            for input in [
                format!(r#"{{"opts":{{"{key}":"hunter2hunter2"}}}}"#),
                format!(r#"{{"{key}":"hunter2hunter2"}}"#),
                format!(r#"{{"a":{{"b":[{{"{key}":"hunter2hunter2"}}]}}}}"#),
            ] {
                let view = from_stream("mcp__x__run", &input);
                let wire = json(&view);
                assert!(!wire.contains("hunter2"), "{input} gave {wire}");
                assert_eq!(view.withheld, Some("the step holds a secret"), "{input}");
            }
        }
        let plain = from_stream("mcp__x__run", r#"{"opts":{"name":"hunter2hunter2"}}"#);
        assert_eq!(plain.withheld, None);
    }

    #[test]
    fn a_bare_sensitive_file_in_a_list_under_a_path_key_is_withheld() {
        for file in ["id_rsa", "credentials", "x.pem"] {
            let view = from_stream(
                "mcp__x__read",
                &format!(r#"{{"opts":{{"paths":["{file}"]}}}}"#),
            );
            assert!(view.withheld.is_some(), "{file}");
        }
        assert_eq!(leaf_name("opts.paths[0]"), "paths");
        assert_eq!(leaf_name("edits[0].path"), "path");
    }

    #[test]
    fn a_step_naming_a_sensitive_path_shows_only_its_path_whatever_its_kind() {
        for (tool, input) in [
            (
                "mcp__fs__write_file",
                r#"{"path":"/w/.env","content":"DB=plain"}"#,
            ),
            ("Strange", r#"{"file_path":"/w/.env","body":"DB=plain"}"#),
        ] {
            let view = from_stream(tool, input);
            let wire = json(&view);
            assert!(view.withheld.is_some(), "{tool}");
            assert!(!wire.contains("DB=plain"), "{wire}");
            assert_eq!(view.args.len(), 1, "{wire}");
        }
        let ordinary = from_stream(
            "mcp__fs__write_file",
            r#"{"path":"/w/a.txt","content":"x"}"#,
        );
        assert_eq!(ordinary.args.len(), 2);
    }

    #[test]
    fn closing_and_abandoning_a_step() {
        let mut view = open(&Redactor::new(), step(StepKind::Command, "Bash", &[]), 10);
        view.close(&Closing {
            id: "toolu_1".to_owned(),
            failed: true,
            exit: Some(2),
            result_bytes: 40,
            result: None,
            at: 20,
        });
        assert_eq!(view.status, Status::Failed);
        assert_eq!(
            (view.exit, view.result_bytes, view.ended_at),
            (Some(2), Some(40), Some(20))
        );
        let mut open_one = open(&Redactor::new(), step(StepKind::Read, "Read", &[]), 10);
        open_one.abandon(30);
        assert_eq!(open_one.note, Some("the turn ended first"));
        view.abandon(99);
        assert_eq!(view.ended_at, Some(20), "a closed step stays as it ended");
    }
}
