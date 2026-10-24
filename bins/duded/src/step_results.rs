//! The text a turn's steps keep apart from the steps themselves: results, and edit text too long
//! to show in a step. It lives only in this run's memory, under its own bounds, so a result never
//! pushes a step out of the ring and a step never pushes a result out of here.

use std::collections::VecDeque;

use dudes_supervisor::Redactor;
use serde::Serialize;

use crate::secret_patterns::contains_secret;
use crate::steps::{self, KeptEdit, KeptResult, KeptText, Status, StepView};

/// Results kept for one agent's latest turn.
pub(crate) const ENTRIES: usize = 64;
/// The most one agent's kept text holds.
pub(crate) const BYTES: usize = 512 * 1_024;
const BASE_BYTES: usize = 64;

/// What one step kept.
#[derive(Debug)]
pub(crate) struct Kept {
    pub(crate) id: String,
    pub(crate) arrival: u128,
    pub(crate) result: Option<KeptResult>,
    pub(crate) edit: Option<KeptEdit>,
}

impl Kept {
    fn bytes(&self) -> usize {
        BASE_BYTES
            + self.id.len()
            + self.result.as_ref().map_or(0, KeptResult::bytes)
            + self.edit.as_ref().map_or(0, KeptEdit::bytes)
    }
}

/// The kept text of a turn's steps, oldest first.
#[derive(Debug, Default)]
pub(crate) struct ResultStore {
    items: VecDeque<Kept>,
    bytes: usize,
}

impl ResultStore {
    pub(crate) const fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn oldest_arrival(&self) -> Option<u128> {
        self.items.front().map(|kept| kept.arrival)
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Kept> {
        self.items.iter().find(|kept| kept.id == id)
    }

    /// Keeps a step's text. A step that is kept again, once for its edit and once for its
    /// result, joins both in one entry.
    pub(crate) fn keep(
        &mut self,
        id: &str,
        arrival: u128,
        result: Option<KeptResult>,
        edit: Option<KeptEdit>,
    ) {
        if result.is_none() && edit.is_none() {
            return;
        }
        if let Some(at) = self.items.iter().position(|kept| kept.id == id) {
            let mut kept = self.items.remove(at).expect("a position just found");
            self.bytes -= kept.bytes();
            kept.arrival = arrival;
            kept.result = result.or(kept.result);
            kept.edit = edit.or(kept.edit);
            self.push(kept);
        } else {
            self.push(Kept {
                id: id.to_owned(),
                arrival,
                result,
                edit,
            });
        }
    }

    fn push(&mut self, kept: Kept) {
        self.bytes += kept.bytes();
        self.items.push_back(kept);
        while self.items.len() > ENTRIES || self.bytes > BYTES {
            self.drop_oldest();
        }
    }

    pub(crate) fn drop_oldest(&mut self) {
        if let Some(old) = self.items.pop_front() {
            self.bytes -= old.bytes();
        }
    }
}

/// Text sent to the page, and whether the rest was cut.
#[derive(Debug, Serialize)]
pub(crate) struct TextView {
    pub(crate) text: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) cut: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct EditText {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) old: Option<TextView>,
    pub(crate) new: TextView,
}

/// What a fetch of one step answers.
#[derive(Debug)]
pub(crate) enum Fetch {
    /// The step is not in the turn's ring, or its text was pushed out of the store.
    Gone,
    /// The step is still running, so it has no result yet.
    Waiting,
    /// The step's text exists and is not shown, for a reason in plain words.
    Withheld(&'static str),
    Ready {
        result: Option<TextView>,
        edit: Option<EditText>,
    },
}

const MCP_REASON: &str = "results of connected tools are not shown";
const SECRET_REASON: &str = "the text holds a secret";

fn shown(redactor: &Redactor, kept: &KeptText) -> TextView {
    let mut text = kept.text.clone();
    steps::rescrub(redactor, &mut text);
    TextView {
        text,
        cut: kept.cut,
    }
}

/// The text of one step, as a page may see it. The text is scrubbed again with the secrets the
/// daemon knows now, then checked for a secret-shaped value, and only then does a step's own
/// withholding (a sensitive path, a connected tool) decide whether it is shown.
pub(crate) fn fetch(redactor: &Redactor, view: &StepView, kept: Option<&Kept>) -> Fetch {
    if view.kind == "mcp" {
        return Fetch::Withheld(MCP_REASON);
    }
    if view.status == Status::Running {
        return Fetch::Waiting;
    }
    let result = match kept.and_then(|kept| kept.result.as_ref()) {
        Some(KeptResult::Text(text)) => Some(shown(redactor, text)),
        _ => None,
    };
    let edit = kept
        .and_then(|kept| kept.edit.as_ref())
        .map(|edit| EditText {
            old: edit.old.as_ref().map(|old| shown(redactor, old)),
            new: shown(redactor, &edit.new),
        });
    let secret = matches!(
        kept.and_then(|kept| kept.result.as_ref()),
        Some(KeptResult::Secret)
    ) || result
        .as_ref()
        .is_some_and(|result| steps::result_holds_secret(&result.text))
        || edit.as_ref().is_some_and(|edit| {
            contains_secret(edit.new.text.as_bytes())
                || edit
                    .old
                    .as_ref()
                    .is_some_and(|old| contains_secret(old.text.as_bytes()))
        });
    if secret {
        return Fetch::Withheld(SECRET_REASON);
    }
    if let Some(reason) = view.withheld {
        return Fetch::Withheld(reason);
    }
    let owed_result = steps::keeps_result(view) && view.result_bytes.is_some_and(|bytes| bytes > 0);
    let owed_edit = view.edit.as_ref().is_some_and(|edit| edit.cut);
    if (owed_result && result.is_none()) || (owed_edit && edit.is_none()) {
        return Fetch::Gone;
    }
    Fetch::Ready { result, edit }
}
