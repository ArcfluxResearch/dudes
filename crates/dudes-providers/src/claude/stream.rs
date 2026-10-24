//! One managed turn's stdout, accumulated: the point where [`parse_line`], [`turn_outcome`] and
//! [`usage::account`] are finally composed into what `Input::ChildExited` carries.
//!
//! # Why an accumulator rather than a function over the whole stream
//!
//! A managed turn's stdout is unbounded — a long turn prints megabytes of `assistant` lines — and
//! the reader that feeds this cannot hold it: a pipe with nobody reading blocks the child when the
//! buffer fills, so the reader must consume every line as it arrives and may keep only what the
//! end of the turn needs: the **last attributable** `result` line (or the last provisional one
//! before init), the **first** session id, and one usage figure per API call. Everything else is
//! read, counted and dropped.
//!
//! # A `result` line's figures are the session's, a call's figure is its own
//!
//! On Claude Code 2.1.283 a resumed turn's `modelUsage` and `total_cost_usd` are the session's
//! running total, every earlier turn included (`tests/fixtures/claude/README.md`), so the result
//! leaves as [`ScopedUsage::Session`]. The last `assistant` usage figure per message id supplies
//! the session figure's `calls` lower bound. Some captured releases repeat the initial output
//! count on later lines, so this can undercount output tokens as well as omit unseen calls.
//! A session increment below this observed lower bound is charged whole (invariant 4).
//!
//! # A `result` line is the turn's result only if it names the turn's session
//!
//! Invariant 3: `Done` is what the daemon observed, and an agent's self-report is evidence rather
//! than a verdict. A `result` line names an outcome, a final message and a cost, so a line the
//! daemon cannot attribute to the session `system/init` announced is a self-report about some
//! other turn — or about none — and [`TurnStream::finish`] does not take it for this one. Both
//! ids must be present and equal; a stream that announced nothing, and a line that names nothing,
//! each leave the daemon holding one id, and one id is not a match. Nothing is refused here for
//! *disagreeing with what the daemon asked for*: that comparison is the supervisor's, and this
//! module only makes it possible by keeping the two ids apart.
//!
//! The vendor stamps every line of a run with the session it announced, `result` included, which
//! `tests/golden.rs::every_fixture_parses_to_its_recorded_shape` asserts over all five captured
//! runs — so no recorded stream loses its result to this check.
//!
//! # It is still pure
//!
//! No I/O, no clock, no allocation that depends on how fast the child writes. [`TurnStream::push`]
//! takes text somebody else read; [`TurnStream::finish`] takes the exit status somebody else
//! waited for. Two runs over an equal sequence of lines give an equal [`TurnEnd`], which is what
//! makes a golden fixture possible.
//!
//! # A line this adapter cannot read is counted, never fatal
//!
//! A vendor release that changes a payload shape must not cost the turn its end. `parse_line`
//! already keeps an unknown `type` verbatim; what reaches [`TurnEnd::unreadable`] is the narrower
//! set that is not JSON at all or whose *known* payload drifted — and a turn that ends with a
//! readable `result` line still ends normally, with a count beside it for whoever is watching for
//! drift.

use std::collections::BTreeMap;

use dudes_proto::providers::claude::{ResultLine, SystemInit};
use dudes_proto::{ExitStatus, LiveActivityKind};
use dudes_types::{SessionId, Usage};

use crate::claude::outcome::{TurnOutcome, turn_outcome};
use crate::claude::steps::LiveSteps;
use crate::claude::text::LiveText;
use crate::claude::usage;
use crate::claude::{Line, StreamLine, parse_line};
use crate::steps::StepEvent;
use crate::stream::{
    Credential, FinalReply, LineRead, ScopedUsage, SessionObservation, TurnEnd, TurnReader,
};

/// One managed turn's stdout, as far as it has been read.
///
/// **The last attributable `result` line wins, not the last observed line.** The vendor writes one
/// per turn and it is the last line of the stream, so in practice there is only ever one. An
/// unbindable trailing line is counted and dropped without erasing an earlier line this turn can
/// claim. Before init, the last result remains provisional until a session is announced.
#[derive(Debug, Default)]
pub struct TurnStream {
    result: Option<ResultRead>,
    provisional_result: Option<ResultRead>,
    result_observed: bool,
    session: Option<SessionId>,
    init_read: bool,
    calls: BTreeMap<String, Usage>,
    /// The newest main-thread call's model and prompt tokens: what the context held when the turn
    /// last called the model.
    last_call: Option<(String, u64)>,
    attribution_refused: u64,
    unreadable: u64,
    text: LiveText,
    steps: LiveSteps,
}

/// A `result` line as it was read, beside the non-empty session **its own** envelope named —
/// `None` when it named none. The two travel together because the line means nothing without it.
#[derive(Debug)]
struct ResultRead {
    line: ResultLine,
    session: Option<SessionId>,
}

impl ResultRead {
    /// Whether this line is attributable to the session `system/init` announced: both ids present
    /// and equal. See this module's docs for why one id is not a match.
    fn binds_to(&self, announced: Option<&SessionId>) -> bool {
        self.session
            .as_ref()
            .zip(announced)
            .is_some_and(|(named, announced)| named == announced)
    }
}

impl TurnStream {
    /// An empty accumulator, before the child has written anything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read one line of the child's standard output, without its newline.
    ///
    /// Blank lines and lines that are not JSON are counted and dropped rather than refused: this
    /// is a pipe, not a protocol with a framing error to report to, and a vendor that prints one
    /// stray line must not cost the turn its end.
    pub fn push(&mut self, text: &str) {
        let _ = self.push_parsed(text);
    }

    /// Accumulate the turn outcome and return the already parsed line to an output consumer.
    /// A malformed line is counted and never returned as raw JSON.
    fn push_parsed(&mut self, text: &str) -> Option<Line> {
        let Ok(line) = parse_line(text) else {
            self.unreadable += 1;
            return None;
        };
        let Line { envelope, body } = &line;
        // The envelope carries the session id on every line, and `system/init` is the line that
        // *announces* one. Reading it off the envelope of any line would report an
        // id on a turn the vendor never confirmed one for.
        if self.session.is_none()
            && matches!(body, StreamLine::Init(_))
            && let Some(id) = envelope.session_id.as_deref().filter(|id| !id.is_empty())
        {
            self.session = Some(SessionId::new(id));
            if let Some(provisional) = self.provisional_result.take() {
                if provisional.binds_to(self.session.as_ref()) {
                    self.result = Some(provisional);
                } else {
                    self.attribution_refused += 1;
                }
            }
        }
        if let StreamLine::Assistant(message) = body
            && let (Some(id), Some(usage)) = (&message.message.id, &message.message.usage)
        {
            self.calls.insert(id.clone(), usage::call(usage));
            if message.parent_tool_use_id.is_none()
                && message.is_synthetic != Some(true)
                && let Some(model) = message.message.model.as_deref().filter(|m| !m.is_empty())
            {
                self.last_call = Some((model.to_owned(), usage::prompt_tokens(usage)));
            }
        }
        // Before an announcement, keep one provisional line because init may arrive later. Once
        // the session is known, retain only binding candidates; count and drop every other line
        // without letting it erase the latest attributable result.
        if let StreamLine::Result(result) = body {
            self.result_observed = true;
            let read = ResultRead {
                line: result.clone(),
                session: envelope
                    .session_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .map(SessionId::new),
            };
            if self.session.is_none() {
                self.provisional_result = Some(read);
            } else if read.binds_to(self.session.as_ref()) {
                self.result = Some(read);
            } else {
                self.attribution_refused += 1;
            }
        }
        Some(line)
    }

    /// The session the vendor has announced so far.
    #[must_use]
    pub fn session(&self) -> Option<&SessionId> {
        self.session.as_ref()
    }

    /// Whether a `result` line has been read at all — the vendor wrote one, whatever session it
    /// named. An observation about the stream, like [`TurnEnd::unreadable`], and not a claim that
    /// the turn has a result: whether the turn can *claim* the line is [`TurnStream::finish`]'s
    /// narrower question, and a line it refuses ends the turn `NoResult` exactly as no line at
    /// all would while this still answers `true`.
    #[must_use]
    pub const fn has_result(&self) -> bool {
        self.result_observed
    }

    /// Close the turn: the stream is at its end and `exit` is what the platform reported.
    ///
    /// The two payloads `Record::TurnEnded` carries as fields of its own — `Completed`'s final
    /// message and `NoResult`'s exit status — are taken off the provider outcome **before** it
    /// crosses to the wire enum, which is what that crossing's own doc requires of every caller.
    #[must_use]
    pub fn finish(self, exit: ExitStatus) -> TurnEnd {
        let Self {
            result,
            provisional_result,
            result_observed: _,
            session,
            init_read: _,
            calls,
            last_call,
            attribution_refused,
            unreadable,
            text: _,
            steps: _,
        } = self;
        // Anything still provisional never received a non-empty session announcement and is
        // refused now. `result` contains only candidates already bound to the announced session,
        // so every field below derives from attributable evidence under invariant 3.
        let attribution_refused = attribution_refused + u64::from(provisional_result.is_some());
        let result = result.map(|read| read.line);
        let derived = turn_outcome(result.as_ref(), exit);
        let last_message = match &derived {
            TurnOutcome::Completed { last_message } => last_message.clone(),
            _ => None,
        };
        let final_reply = result
            .as_ref()
            .filter(|_| matches!(derived, TurnOutcome::Completed { .. }))
            .and_then(|result| result.result.as_deref())
            .map(FinalReply::new);
        let (usage, usage_refused) = match result.as_ref().map(usage::account) {
            Some(Ok(total)) => (
                Some(ScopedUsage::Session {
                    total,
                    calls: calls.into_values().sum(),
                }),
                None,
            ),
            // Invariant 4: an unusable or partial cost is refused, never rounded down to zero and
            // never summed halfway. The turn still ends — the end is Observed either way — and
            // what the budget does with an end it cannot price is the budget's concern, not this module's.
            Some(Err(error)) => (None, Some(error)),
            None => (None, None),
        };
        // The breakdown comes off the same attributable line as the aggregate, and off nothing
        // else: a `result` the daemon could not bind to the announced session buys no evidence any
        // more than it buys an outcome or a cost.
        let model_usage = result.as_ref().map(usage::by_model).unwrap_or_default();
        let context = result
            .as_ref()
            .zip(last_call)
            .and_then(|(result, (model, used))| usage::context(result, &model, used));
        TurnEnd {
            final_observation: None,
            outcome: derived.into(),
            report: dudes_proto::TurnReport {
                context,
                step_digest: None,
            },
            usage,
            usage_refused,
            model_usage,
            last_message,
            final_reply,
            session,
            attribution_refused,
            unreadable,
        }
    }
}

impl TurnReader for TurnStream {
    fn line(&mut self, text: &str) -> LineRead {
        let Some(line) = self.push_parsed(text) else {
            // The host cannot tell a blank line from one it would not keep, so neither may end
            // a step. The end of the turn settles any that stay open.
            return LineRead::default();
        };
        let text = self
            .text
            .read(&line, self.session.as_ref())
            .unwrap_or_else(|_| {
                self.unreadable += 1;
                Vec::new()
            });
        let steps = self.steps.read(&line, self.session.as_ref());
        let activity = activity(&line, &steps, !text.is_empty());
        let session = match line.body {
            StreamLine::Init(init) if !self.init_read => {
                self.init_read = true;
                Some(observe(self.session.clone(), init))
            }
            _ => None,
        };
        LineRead {
            actions: Vec::new(),
            omitted_text: self
                .text
                .take_omitted_text()
                .then(|| self.session.clone())
                .flatten(),
            text,
            session,
            activity,
            steps,
        }
    }

    fn end(self: Box<Self>, exit: ExitStatus) -> TurnEnd {
        self.finish(exit)
    }
}

/// The fixed kind a line shows to viewers who may not see its content.
fn activity(line: &Line, steps: &[StepEvent], text: bool) -> Option<LiveActivityKind> {
    if steps
        .iter()
        .any(|step| matches!(step, StepEvent::Open { .. }))
    {
        return Some(LiveActivityKind::UsingTool);
    }
    match &line.body {
        StreamLine::Init(_) => Some(LiveActivityKind::Working),
        StreamLine::System { raw, .. }
            if raw.get("status").and_then(serde_json::Value::as_str) == Some("compacting") =>
        {
            Some(LiveActivityKind::Compacting)
        }
        _ if text || !steps.is_empty() => Some(LiveActivityKind::Working),
        _ => None,
    }
}

/// `"none"` is Claude's word for no API key. Built-in plugins are left out.
fn observe(session: Option<SessionId>, init: SystemInit) -> SessionObservation {
    SessionObservation {
        session,
        credential: init.api_key_source.map(|source| {
            if source == "none" {
                Credential::NoApiKey
            } else {
                Credential::ApiKey(source)
            }
        }),
        connected: Some(
            init.mcp_servers
                .iter()
                .filter(|server| server.status == "connected")
                .map(|server| server.name.clone())
                .collect(),
        ),
        servers: init
            .mcp_servers
            .into_iter()
            .map(|server| server.name)
            .collect(),
        plugins: init
            .plugins
            .iter()
            .filter(|plugin| !built_in(plugin))
            .map(|plugin| {
                plugin
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| plugin.as_str())
                    .map(str::to_owned)
            })
            .collect(),
    }
}

/// The exact built-in path and source distinguish bundled plugins from installed namesakes.
fn built_in(plugin: &serde_json::Value) -> bool {
    let field = |key| plugin.get(key).and_then(serde_json::Value::as_str);
    field("path") == Some("builtin")
        && field("name")
            .zip(field("source"))
            .is_some_and(|(name, source)| source.strip_suffix("@builtin") == Some(name))
}

#[cfg(test)]
mod tests {
    use dudes_types::Tokens;

    use super::*;
    use crate::stream::{TextBlock, UsageError};

    const EXIT_0: ExitStatus = ExitStatus {
        code: Some(0),
        signal: None,
    };

    /// The session `system/init` announces in every stream built here.
    const SESSION: &str = "3f9a1c2e-0000-4000-8000-000000000001";

    /// A session this turn never announced: the id a crossed or resumed sibling's stray `result`
    /// line carries, and the one a line from anywhere else would have to guess.
    const OTHER: &str = "3f9a1c2e-0000-4000-8000-000000000002";

    /// `system/init`, the line that announces `session`.
    fn init(session: &str) -> String {
        [
            r#"{"type":"system","subtype":"init","session_id":""#,
            session,
            r#"","tools":[]}"#,
        ]
        .concat()
    }

    fn observed(lines: &[&str]) -> Vec<Option<SessionObservation>> {
        let mut reader: Box<dyn TurnReader> = Box::new(TurnStream::new());
        lines.iter().map(|line| reader.line(line).session).collect()
    }

    /// The init's whole server list, and the servers on it that connected, so the supervisor can
    /// tell an undeclared server from a declared one that failed. Mutant: report only connected
    /// servers, which would hide an undeclared server that failed to connect.
    #[test]
    fn an_init_reports_every_listed_server_and_which_connected() {
        let line = format!(
            r#"{{"type":"system","subtype":"init","session_id":"{SESSION}","mcp_servers":[{{"name":"memory","status":"connected"}},{{"name":"rogue","status":"failed"}},{{"name":"notes","status":"pending"}}]}}"#
        );
        let observation = observed(&[&line])[0]
            .clone()
            .expect("an init opens the session");
        assert_eq!(observation.servers, ["memory", "rogue", "notes"]);
        assert_eq!(observation.connected, Some(vec!["memory".to_owned()]));
    }

    #[test]
    fn the_first_init_opens_the_session_once() {
        let first = format!(
            r#"{{"type":"system","subtype":"init","session_id":"{SESSION}","apiKeySource":"none","mcp_servers":[{{"name":"repository-tools","status":"connected"}}],"plugins":[{{"name":"memory"}},"bare",{{"path":"/plugins/x"}}]}}"#
        );
        assert_eq!(
            observed(&[&first, &init(OTHER)]),
            [
                Some(SessionObservation {
                    session: Some(SessionId::new(SESSION)),
                    credential: Some(Credential::NoApiKey),
                    servers: vec!["repository-tools".to_owned()],
                    connected: Some(vec!["repository-tools".to_owned()]),
                    plugins: vec![Some("memory".to_owned()), Some("bare".to_owned()), None],
                }),
                None,
            ]
        );
    }

    #[test]
    fn an_unfamiliar_builtin_is_left_out_but_its_installed_namesake_is_listed() {
        let line = format!(
            r#"{{"type":"system","subtype":"init","session_id":"{SESSION}","plugins":[{{"name":"optional-helper","path":"builtin","source":"optional-helper@builtin"}},{{"name":"optional-helper","path":"/plugins/optional-helper","source":"optional-helper@builtin"}}]}}"#
        );
        assert_eq!(
            observed(&[&line])[0].as_ref().unwrap().plugins,
            [Some("optional-helper".to_owned())]
        );
    }

    /// Only the exact shape of a built-in entry is left out: a marketplace named `builtin` still
    /// installs to a directory, and a `builtin` path under another plugin's name or another
    /// marketplace is not the vendor's own.
    #[test]
    fn a_built_in_plugin_is_left_out_and_every_other_is_listed() {
        let line = format!(
            r#"{{"type":"system","subtype":"init","session_id":"{SESSION}","plugins":[{{"name":"agents-md","path":"builtin","source":"agents-md@builtin"}},{{"name":"telemetry","path":"builtin","source":"telemetry@builtin"}},{{"name":"memory","path":"/tmp/cfg/.claude/plugins/cache/builtin/memory/1.0.0","source":"memory@builtin"}},{{"name":"lint","path":"builtin","source":"lint@team"}},{{"name":"docs","path":"builtin","source":"agents-md@builtin"}}]}}"#
        );
        let plugins = observed(&[&line])[0]
            .as_ref()
            .map(|observation| observation.plugins.clone());
        assert_eq!(
            plugins,
            Some(vec![
                Some("memory".to_owned()),
                Some("lint".to_owned()),
                Some("docs".to_owned()),
            ])
        );
    }

    #[test]
    fn a_malformed_builtin_entry_is_kept_as_isolation_evidence() {
        use serde_json::json;

        for (plugin, name) in [
            (json!({"path":"builtin","source":"agents-md@builtin"}), None),
            (
                json!({"name":false,"path":"builtin","source":"agents-md@builtin"}),
                None,
            ),
            (
                json!({"name":"agents-md","source":"agents-md@builtin"}),
                Some("agents-md"),
            ),
            (
                json!({"name":"agents-md","path":false,"source":"agents-md@builtin"}),
                Some("agents-md"),
            ),
            (
                json!({"name":"agents-md","path":"builtin"}),
                Some("agents-md"),
            ),
            (
                json!({"name":"agents-md","path":"builtin","source":false}),
                Some("agents-md"),
            ),
        ] {
            let line = json!({
                "type":"system", "subtype":"init", "session_id":SESSION, "plugins":[plugin]
            })
            .to_string();
            assert_eq!(
                observed(&[&line])[0].as_ref().unwrap().plugins,
                [name.map(str::to_owned)],
                "{line}"
            );
        }
    }

    #[test]
    fn an_empty_first_session_opens_with_no_id() {
        assert_eq!(
            observed(&[&init(""), &init(SESSION)]),
            [
                Some(SessionObservation {
                    connected: Some(Vec::new()),
                    ..SessionObservation::default()
                }),
                None
            ]
        );
    }

    #[test]
    fn every_reported_source_but_none_is_an_api_key() {
        let reported = |source: &str| {
            let line = format!(
                r#"{{"type":"system","subtype":"init","session_id":"{SESSION}","apiKeySource":"{source}"}}"#
            );
            observed(&[&line])[0]
                .as_ref()
                .and_then(|observation| observation.credential.clone())
        };
        for source in ["ANTHROPIC_API_KEY", "apiKeyHelper", "/login managed key"] {
            assert_eq!(
                reported(source),
                Some(Credential::ApiKey(source.to_owned())),
                "{source:?}"
            );
        }
        assert_eq!(reported("none"), Some(Credential::NoApiKey));
        assert_eq!(
            observed(&[&init(SESSION)])[0]
                .as_ref()
                .and_then(|observation| observation.credential.clone()),
            None,
            "an init without apiKeySource reports no credential"
        );
    }

    #[test]
    fn assistant_text_is_read_as_blocks() {
        let mut reader: Box<dyn TurnReader> = Box::new(TurnStream::new());
        let line = format!(
            r#"{{"type":"assistant","session_id":"{SESSION}","message":{{"role":"assistant","content":[{{"type":"text","text":"visible"}},{{"type":"thinking","thinking":"hidden"}}]}}}}"#
        );
        assert_eq!(
            reader.line(&line),
            LineRead {
                actions: Vec::new(),
                omitted_text: None,
                text: vec![TextBlock {
                    session: SessionId::new(SESSION),
                    text: "visible".to_owned(),
                }],
                session: None,
                activity: Some(LiveActivityKind::Working),
                steps: Vec::new(),
            }
        );
    }

    /// A `result` line carrying a cost and per-model tokens, stamped with `session` — or, when
    /// that is `None`, carrying no `session_id` field at all. Every fragment is vendor text as
    /// captured; only the id varies, so a test that changes it changes nothing else.
    fn result(session: Option<&str>) -> String {
        result_with(session, "done", "0.25")
    }

    /// A result whose outcome identity differs visibly from the ordinary fixture, used to prove
    /// that a rejected trailing line contributes neither its message nor its cost.
    fn result_with(session: Option<&str>, message: &str, total_cost_usd: &str) -> String {
        let stamp = session.map_or_else(String::new, |id| {
            [r#""session_id":""#, id, r#"","#].concat()
        });
        [
            r#"{"type":"result","subtype":"success","is_error":false,"#,
            &stamp,
            r#""result":""#,
            message,
            r#"","total_cost_usd":"#,
            total_cost_usd,
            r",",
            r#""modelUsage":{"claude-opus-4":{"inputTokens":11,"outputTokens":22,"cacheReadInputTokens":33,"cacheCreationInputTokens":44}}}"#,
        ]
        .concat()
    }

    /// A minimal stream: an `init` line that announces a session, one `assistant` line, and a
    /// `result` line carrying a cost and per-model tokens. All three name the same session, which
    /// is what every captured run in `tests/fixtures` does.
    fn stream() -> Vec<String> {
        vec![
            init(SESSION),
            [
                r#"{"type":"assistant","session_id":""#,
                SESSION,
                r#"","message":{"content":[]}}"#,
            ]
            .concat(),
            result(Some(SESSION)),
        ]
    }

    fn read(lines: &[String]) -> TurnStream {
        let mut turn = TurnStream::new();
        for line in lines {
            turn.push(line);
        }
        turn
    }

    /// A managed turn ending normally carries `Some(Usage)`. The whole composition —
    /// `parse_line` → the last `result` line → `usage::account` — in one assertion, with the
    /// tokens checked as well as the cost so that a `Usage::ZERO` returned by a broken sum cannot
    /// pass for an accounted one.
    #[test]
    fn a_normal_end_carries_usage() {
        let end = read(&stream()).finish(EXIT_0);
        let Some(ScopedUsage::Session { total: usage, .. }) = end.usage else {
            panic!(
                "a result line with a cost accounts to this turn's usage: {:?}",
                end.usage
            );
        };
        assert_eq!(usage.usd, Some(dudes_types::Usd::from_micros(250_000)));
        assert_eq!(usage.input.count(), 11);
        assert_eq!(usage.output.count(), 22);
        assert_eq!(usage.cache_read.count(), 33);
        assert_eq!(usage.cache_creation.count(), 44);
        assert_eq!(end.outcome, dudes_proto::TurnOutcome::Completed);
        assert_eq!(end.last_message.as_deref(), Some("done"));
        assert_eq!(end.usage_refused.map(|e| e.to_string()), None);
        assert_eq!(end.unreadable, 0);
    }

    /// The control on the assertion above: the same reader over the same stream **without** its
    /// `result` line ends `NoResult` with no usage at all. Without this, a `finish` that invented
    /// a usage from nothing would pass the test above.
    #[test]
    fn a_stream_with_no_result_carries_none() {
        let lines = stream();
        let end = read(&lines[..2]).finish(EXIT_0);
        assert_eq!(end.usage, None);
        assert_eq!(end.outcome, dudes_proto::TurnOutcome::NoResult);
        assert_eq!(end.last_message, None);
    }

    /// A `result` line naming a session other than the one `system/init` announced is
    /// not this turn's result. Invariant 3 — the daemon cannot attribute the line, so it cannot
    /// take it for an observation of its own turn: no usage, no final message, and the turn ends
    /// on the exit status alone. `usage_refused` stays `None` because no cost was ever considered,
    /// which is what tells this refusal apart from a cost that was read and could not be charged
    /// (`an_uncharged_cost_is_refused_and_says_so`).
    ///
    /// **The breakdown is refused with the rest of it**: a result that is not
    /// attributable to this turn contributes neither aggregate usage nor a map. This is the arm
    /// that carries that clause, because `has_result()` above says the line was *read* and then
    /// refused — so an empty map here is the refusal doing work rather than a line nobody saw.
    /// Computing the breakdown from the pre-filter `result` would leave this line's one
    /// `modelUsage` key on the end.
    #[test]
    fn a_result_line_naming_another_session_is_not_the_turns_result() {
        let mut lines = stream();
        lines[2] = result(Some(OTHER));
        let turn = read(&lines);
        assert!(
            turn.has_result(),
            "the line was read; what refuses it is the session it names"
        );
        let end = turn.finish(EXIT_0);
        assert_eq!(end.usage, None);
        assert_eq!(end.usage_refused.map(|e| e.to_string()), None);
        assert_eq!(end.attribution_refused, 1);
        assert_eq!(end.outcome, dudes_proto::TurnOutcome::NoResult);
        assert_eq!(end.last_message, None);
        assert_eq!(end.unreadable, 0, "the line was read, not unreadable");
        assert!(
            end.model_usage.is_empty(),
            "a line the turn cannot claim contributes no breakdown either: {:?}",
            end.model_usage
        );
        assert_eq!(
            end.session.as_ref().map(SessionId::as_str),
            Some(SESSION),
            "the announcement is still what the turn reports"
        );

        // Positive control, the same stream with only that id put back: the adapter is not
        // passing the assertions above by refusing every `result` line it is given.
        let matching = read(&stream()).finish(EXIT_0);
        assert_eq!(matching.attribution_refused, 0);
        assert!(
            matching.usage.is_some(),
            "control: the announced id still accounts"
        );
        assert!(
            !matching.model_usage.is_empty(),
            "control: the announced id still breaks down, so the empty map above is a refusal \
             and not a map this stream never had"
        );
        assert_eq!(matching.outcome, dudes_proto::TurnOutcome::Completed);
        assert_eq!(matching.last_message.as_deref(), Some("done"));
    }

    /// The same attribution check with nothing on one side: a `result` line carrying no `session_id` at all
    /// names no session, so it is attributable to none. The vendor stamps every line of every
    /// captured run, `result` included, so a line without one came from somewhere else.
    #[test]
    fn a_result_line_naming_no_session_is_not_the_turns_result() {
        let mut lines = stream();
        lines[2] = result(None);
        let end = read(&lines).finish(EXIT_0);
        assert_eq!(end.usage, None);
        assert_eq!(end.usage_refused.map(|e| e.to_string()), None);
        assert_eq!(end.outcome, dudes_proto::TurnOutcome::NoResult);
        assert_eq!(end.last_message, None);
        assert!(
            end.model_usage.is_empty(),
            "a line the turn cannot claim contributes no breakdown either: {:?}",
            end.model_usage
        );

        // Positive control: the same line stamped with the announced id accounts.
        let matching = read(&stream()).finish(EXIT_0);
        assert!(
            matching.usage.is_some(),
            "control: the announced id still accounts"
        );
        assert!(
            !matching.model_usage.is_empty(),
            "control: the announced id still breaks down"
        );
        assert_eq!(matching.outcome, dudes_proto::TurnOutcome::Completed);
    }

    /// And nothing on the other side: with no `init` line, nothing was announced, so a `result`
    /// line that names a session is attributable to none of them — one id is not a match,
    /// whichever half is missing.
    #[test]
    fn a_result_line_is_not_the_turns_result_when_nothing_was_announced() {
        let lines = stream();
        let end = read(&lines[1..]).finish(EXIT_0);
        assert_eq!(end.session, None, "no init line, so nothing was announced");
        assert_eq!(end.usage, None);
        assert_eq!(end.outcome, dudes_proto::TurnOutcome::NoResult);
        assert_eq!(end.last_message, None);
        assert!(
            end.model_usage.is_empty(),
            "a line the turn cannot claim contributes no breakdown either: {:?}",
            end.model_usage
        );

        // Positive control: the same lines with the announcement in front of them account.
        let announced = read(&lines).finish(EXIT_0);
        assert!(
            announced.usage.is_some(),
            "control: the announcement makes the same line the turn's result"
        );
        assert!(
            !announced.model_usage.is_empty(),
            "control: and gives it a breakdown"
        );
    }

    /// The session is read from the line that **announces** one, and only from it: an `assistant`
    /// line carrying the same id in its envelope does not announce a session, so a
    /// stream whose `init` never arrived reports none even though every other line names it.
    #[test]
    fn only_an_init_line_announces_the_session() {
        let lines = stream();
        assert_eq!(
            read(&lines).session.as_ref().map(SessionId::as_str),
            Some("3f9a1c2e-0000-4000-8000-000000000001")
        );
        assert_eq!(read(&lines[1..]).session, None);
    }

    /// An empty id is absent on both sides of the attribution check: it is neither a session the
    /// vendor confirmed nor a stamp that can bind a result to this turn.
    #[test]
    fn empty_session_ids_are_absent_and_do_not_bind() {
        let turn = read(&[init(""), result(Some(""))]);
        assert_eq!(turn.session(), None);
        let end = turn.finish(EXIT_0);
        assert_eq!(end.session, None);
        assert_eq!(end.attribution_refused, 1);
        assert_eq!(end.usage, None);
        assert_eq!(end.outcome, dudes_proto::TurnOutcome::NoResult);

        let matching = read(&stream()).finish(EXIT_0);
        assert_eq!(
            matching.session.as_ref().map(SessionId::as_str),
            Some(SESSION),
            "control: a real id is still announced"
        );
        assert_eq!(matching.attribution_refused, 0);
        assert!(matching.usage.is_some(), "control: a real id still binds");
    }

    /// A line this adapter cannot read costs a count and never the turn: the `result` line that
    /// follows still ends it, with usage.
    #[test]
    fn unreadable_lines_are_counted_not_fatal() {
        let mut lines = vec!["not json".to_owned(), String::new()];
        lines.extend(stream());
        let end = read(&lines).finish(EXIT_0);
        assert_eq!(end.unreadable, 2);
        assert!(end.usage.is_some(), "the turn still ends with usage");
    }

    /// A blank line is also what the host delivers for a line it would not keep, so it cannot
    /// close a step: only the call's own result does.
    #[test]
    fn a_blank_line_leaves_an_open_step_open() {
        let mut stream = TurnStream::default();
        let call = format!(
            r#"{{"type":"assistant","session_id":"{SESSION}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_1","name":"Bash","input":{{"command":"ls"}}}}]}}}}"#
        );
        let result = format!(
            r#"{{"type":"user","session_id":"{SESSION}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}}]}}}}"#
        );
        assert!(stream.line(&init(SESSION)).steps.is_empty());
        assert_eq!(stream.line(&call).steps.len(), 1);
        assert!(
            stream.line("").steps.is_empty(),
            "a blank line closes nothing"
        );
        let closed = stream.line(&result).steps;
        assert!(matches!(
            &closed[..],
            [StepEvent::Close { failed: false, .. }]
        ));
    }

    /// The gauge's numbers come from the last main-thread call's prompt and the window the result
    /// names for that call's model. A helper agent's later call and a window the result does not
    /// report change nothing.
    #[test]
    fn context_is_the_last_main_call_against_its_models_window() {
        let call = |id: &str, model: &str, parent: &str, input: u64| {
            format!(
                r#"{{"type":"assistant","session_id":"{SESSION}",{parent}"message":{{"id":"{id}","model":"{model}","role":"assistant","content":[],"usage":{{"input_tokens":{input},"output_tokens":1,"cache_read_input_tokens":100,"cache_creation_input_tokens":10}}}}}}"#
            )
        };
        let result = |window: &str| {
            format!(
                r#"{{"type":"result","subtype":"success","is_error":false,"session_id":"{SESSION}","result":"ok","total_cost_usd":0.1,"modelUsage":{{"opus-x":{{"inputTokens":9,"outputTokens":1,"cacheReadInputTokens":1,"cacheCreationInputTokens":1,"costUSD":0.1{window}}},"haiku-x":{{"inputTokens":1,"outputTokens":1,"cacheReadInputTokens":1,"cacheCreationInputTokens":1,"costUSD":0.0,"contextWindow":1000}}}}}}"#
            )
        };
        let lines = |window: &str| {
            read(&[
                init(SESSION),
                call("a", "opus-x", "", 1),
                call("b", "opus-x", "", 5),
                call("c", "haiku-x", r#""parent_tool_use_id":"toolu_1","#, 999),
                result(window),
            ])
            .finish(EXIT_0)
            .report
            .context
        };
        assert_eq!(
            lines(r#","contextWindow":200000"#),
            Some(dudes_proto::ContextReport {
                model: "opus-x".into(),
                window: 200_000,
                used: 115,
            })
        );
        assert_eq!(lines(""), None, "no reported window, no claim");
    }

    /// A turn can end on a synthetic assistant message: model `<synthetic>`, zero usage. It is not
    /// a call, so the gauge keeps the previous real call, and has no claim when there is none.
    #[test]
    fn a_synthetic_final_message_does_not_become_the_context_call() {
        let call = |id: &str, model: &str, synthetic: &str, input: u64| {
            format!(
                r#"{{"type":"assistant","session_id":"{SESSION}",{synthetic}"message":{{"id":"{id}","model":"{model}","role":"assistant","content":[],"usage":{{"input_tokens":{input},"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#
            )
        };
        let result = format!(
            r#"{{"type":"result","subtype":"success","is_error":false,"session_id":"{SESSION}","result":"ok","total_cost_usd":0.1,"modelUsage":{{"opus-x":{{"inputTokens":9,"outputTokens":1,"cacheReadInputTokens":1,"cacheCreationInputTokens":1,"costUSD":0.1,"contextWindow":200000}}}}}}"#
        );
        let context = |lines: &[String]| {
            let mut all = vec![init(SESSION)];
            all.extend_from_slice(lines);
            all.push(result.clone());
            read(&all).finish(EXIT_0).report.context
        };
        let synthetic = call("s", "<synthetic>", r#""isSynthetic":true,"#, 0);
        assert_eq!(
            context(&[call("a", "opus-x", "", 5), synthetic.clone()]),
            Some(dudes_proto::ContextReport {
                model: "opus-x".into(),
                window: 200_000,
                used: 5,
            })
        );
        assert_eq!(context(&[synthetic]), None);
    }

    /// A streamed message repeats its id on each block's line, and the last line's figure is the
    /// call's: two calls whose first lines report 1 output token and whose last lines report 7 and
    /// 9 add up to 16. Hand-built, because every captured call printed one line.
    #[test]
    fn each_call_counts_once_at_its_last_figure() {
        let call = |id: &str, output: u64| {
            format!(
                r#"{{"type":"assistant","session_id":"{SESSION}","message":{{"id":"{id}","role":"assistant","content":[],"usage":{{"input_tokens":2,"output_tokens":{output},"cache_read_input_tokens":10,"cache_creation_input_tokens":5}}}}}}"#
            )
        };
        let end = read(&[
            init(SESSION),
            call("msg_a", 1),
            call("msg_a", 7),
            call("msg_b", 1),
            call("msg_b", 9),
            result(Some(SESSION)),
        ])
        .finish(EXIT_0);
        let Some(ScopedUsage::Session { calls, .. }) = end.usage else {
            panic!("a result line leaves as a session figure: {:?}", end.usage);
        };
        assert_eq!(
            calls,
            Usage {
                input: Tokens::new(4),
                output: Tokens::new(16),
                cache_read: Tokens::new(20),
                cache_creation: Tokens::new(10),
                usd: None,
            }
        );
    }

    /// Invariant 4: a cost that cannot be charged is refused rather than rounded to zero, and the
    /// refusal is visible — `usage: None` beside `usage_refused: Some`, which is what tells an
    /// unpriced end apart from a turn that reported no cost at all.
    ///
    /// The line names the announced session, so it *is* this turn's result and what is refused
    /// here is the cost alone — reaching `InvalidCost` at all is what says the line was claimed.
    /// That is the difference the attribution tests above rest on: they reach `usage: None` with
    /// `usage_refused: None`, because a line the turn cannot claim never gets as far as its cost.
    #[test]
    fn an_uncharged_cost_is_refused_and_says_so() {
        let mut turn = TurnStream::new();
        turn.push(&init(SESSION));
        turn.push(
            &[
                r#"{"type":"result","subtype":"success","is_error":false,"session_id":""#,
                SESSION,
                r#"","total_cost_usd":-1.0}"#,
            ]
            .concat(),
        );
        let end = turn.finish(EXIT_0);
        assert_eq!(end.usage, None);
        assert!(matches!(
            end.usage_refused,
            Some(UsageError::InvalidCost(_))
        ));
    }

    /// The last `result` line wins, so a stream that somehow carried two closes on the second.
    /// Both name the announced session; one that did not would not be a candidate at all.
    #[test]
    fn the_last_result_line_wins() {
        let line = |message: &str| {
            [
                r#"{"type":"result","subtype":"success","is_error":false,"session_id":""#,
                SESSION,
                r#"","result":""#,
                message,
                r#""}"#,
            ]
            .concat()
        };
        let mut turn = TurnStream::new();
        turn.push(&init(SESSION));
        turn.push(&line("first"));
        turn.push(&line("second"));
        assert_eq!(turn.finish(EXIT_0).last_message.as_deref(), Some("second"));
    }

    /// A later line from another session is refused without erasing the most recent result this
    /// turn can claim. The refusal is still counted even though the retained result completes the
    /// turn.
    #[test]
    fn a_trailing_unbindable_result_does_not_erase_a_binding_result() {
        let mut turn = TurnStream::new();
        turn.push(&init(SESSION));
        turn.push(&result(Some(SESSION)));
        turn.push(&result_with(Some(OTHER), "foreign", "0.75"));
        assert!(turn.has_result(), "both result lines were observed");

        let end = turn.finish(EXIT_0);
        assert_eq!(end.outcome, dudes_proto::TurnOutcome::Completed);
        assert_eq!(end.last_message.as_deref(), Some("done"));
        let Some(ScopedUsage::Session { total: usage, .. }) = end.usage else {
            panic!(
                "the binding result accounts this turn's usage: {:?}",
                end.usage
            );
        };
        assert_eq!(usage.usd, Some(dudes_types::Usd::from_micros(250_000)));
        assert_eq!(end.attribution_refused, 1);
    }

    /// `has_result` reports the observed stream independently of whether the turn can retain an
    /// attributable candidate from it.
    #[test]
    fn an_unbound_only_result_is_observed_but_not_used() {
        let turn = read(&[init(SESSION), result(Some(OTHER))]);
        assert!(
            turn.has_result(),
            "the unbound result line was still observed"
        );

        let end = turn.finish(EXIT_0);
        assert_eq!(end.outcome, dudes_proto::TurnOutcome::NoResult);
        assert_eq!(end.last_message, None);
        assert_eq!(end.usage, None);
        assert_eq!(end.attribution_refused, 1);
    }

    /// A binding result can arrive before the init announcement. It remains provisional until
    /// the first non-empty init reconciles it, then survives a later result from another session.
    #[test]
    fn a_pre_init_binding_result_is_retained_after_announcement() {
        let mut turn = TurnStream::new();
        turn.push(&result(Some(SESSION)));
        turn.push(&init(SESSION));
        turn.push(&result_with(Some(OTHER), "foreign", "0.75"));

        let end = turn.finish(EXIT_0);
        assert_eq!(end.outcome, dudes_proto::TurnOutcome::Completed);
        assert_eq!(end.last_message.as_deref(), Some("done"));
        let Some(ScopedUsage::Session { total: usage, .. }) = end.usage else {
            panic!(
                "the binding result accounts this turn's usage: {:?}",
                end.usage
            );
        };
        assert_eq!(usage.usd, Some(dudes_types::Usd::from_micros(250_000)));
        assert_eq!(end.attribution_refused, 1);
    }
}
