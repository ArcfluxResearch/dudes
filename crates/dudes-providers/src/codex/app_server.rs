//! One owned Codex app-server process, bounded to one immutable issued task.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use dudes_proto::providers::codex_app_server as wire;
use dudes_proto::{ExitStatus, LiveActivityKind, TurnOutcome};
use dudes_types::{SessionId, Tokens, Usage};
use serde::de::DeserializeOwned;
use serde_json::json;

use super::startup::CodexStartup;
use crate::adapter::TurnSession;
use crate::protocol::{ProtocolAction, StartupFrame};
use crate::stream::{
    FinalReply, LineRead, ScopedUsage, SessionObservation, TextBlock, TextLayout, TextProgress,
    TurnEnd, TurnReader, exit_name,
};

const MAX_EARLY_EVENTS: usize = 128;
const MAX_EARLY_BYTES: usize = 256 * 1024;
const MAX_ITEMS: usize = 128;
const MAX_ID_BYTES: usize = 256;

/// Incremental reader for the opt-in app-server transport.
pub struct AppServerReader {
    startup: CodexStartup,
    phase: Phase,
    thread: Option<SessionId>,
    turn: Option<String>,
    early: Vec<Event>,
    early_bytes: usize,
    items: BTreeMap<String, TextProgress>,
    layout: TextLayout,
    final_reply: Option<FinalReply>,
    usage: Option<Usage>,
    unreadable: u64,
    refused: u64,
    refusal: Option<String>,
    servers: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    New,
    Initializing,
    OpeningThread,
    StartingTurn,
    Active,
    Completed,
    Failed,
}

enum Event {
    Item {
        completed: bool,
        body: wire::ItemNotification,
    },
    Delta(wire::AgentDelta),
    Usage(wire::TokenUsage),
    End(wire::TurnResponse),
}

impl fmt::Debug for AppServerReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppServerReader")
            .field("phase", &self.phase)
            .field("early_events", &self.early.len())
            .field("items", &self.items.len())
            .finish_non_exhaustive()
    }
}

impl AppServerReader {
    /// Reader bound to the exact launch's task and profile.
    #[must_use]
    pub fn new(startup: CodexStartup) -> Self {
        Self {
            startup,
            phase: Phase::New,
            thread: None,
            turn: None,
            early: Vec::new(),
            early_bytes: 0,
            items: BTreeMap::new(),
            layout: TextLayout::default(),
            final_reply: None,
            usage: None,
            unreadable: 0,
            refused: 0,
            refusal: None,
            servers: BTreeSet::new(),
        }
    }

    fn fail(&mut self) -> LineRead {
        self.phase = Phase::Failed;
        self.early.clear();
        self.early_bytes = 0;
        LineRead {
            actions: vec![ProtocolAction::Fail],
            ..LineRead::default()
        }
    }

    fn response(&mut self, id: u64, text: &str) -> LineRead {
        match (self.phase, id) {
            (Phase::Initializing, 1) => self.initialized(text),
            (Phase::OpeningThread, 2) => {
                let Some(body) = response::<wire::ThreadResponse>(text) else {
                    return self.fail();
                };
                if !self.posture(&body) {
                    return self.fail();
                }
                let Some(id) = body
                    .thread
                    .filter(|thread| {
                        thread
                            .status
                            .as_ref()
                            .and_then(|status| status.kind.as_deref())
                            == Some("idle")
                    })
                    .and_then(|thread| thread.id)
                    .and_then(|id| super::stream::thread_id(&id))
                else {
                    return self.fail();
                };
                if matches!(&self.startup.session, TurnSession::Resume(expected) if *expected != id)
                {
                    return self.fail();
                }
                let mut params = json!({"threadId":id.as_str(),"input":[{"type":"text","text":self.startup.task,"text_elements":[]}],"cwd":self.startup.cwd,"approvalPolicy":"never","sandboxPolicy":{"type":"workspaceWrite","writableRoots":self.startup.writable_roots(),"networkAccess":false,"excludeTmpdirEnvVar":true,"excludeSlashTmp":true}});
                if let Some(model) = self.startup.selection.selector() {
                    params["model"] = model.into();
                }
                let Some(request) =
                    frame(&json!({"id":3,"method":"turn/start","params":params}), true)
                else {
                    return self.fail();
                };
                self.thread = Some(id);
                self.phase = Phase::StartingTurn;
                LineRead {
                    actions: vec![ProtocolAction::Write(request)],
                    ..LineRead::default()
                }
            }
            (Phase::StartingTurn, 3) => {
                let Some(turn) = response::<wire::TurnResponse>(text).and_then(|body| body.turn)
                else {
                    return self.fail();
                };
                let Some(id) = turn.id.filter(|id| valid_id(id)) else {
                    return self.fail();
                };
                if turn.error.is_some() || turn.status.as_deref() != Some("inProgress") {
                    return self.fail();
                }
                self.turn = Some(id);
                self.phase = Phase::Active;
                let mut read = LineRead {
                    actions: vec![ProtocolAction::Started],
                    session: Some(SessionObservation {
                        session: self.thread.clone(),
                        ..SessionObservation::default()
                    }),
                    activity: Some(LiveActivityKind::Working),
                    ..LineRead::default()
                };
                self.early_bytes = 0;
                for event in std::mem::take(&mut self.early) {
                    merge(&mut read, self.event(event));
                }
                read
            }
            _ => {
                self.refused += 1;
                LineRead::default()
            }
        }
    }

    fn initialized(&mut self, text: &str) -> LineRead {
        let Some(body) = response::<wire::Initialized>(text) else {
            return self.fail();
        };
        if body.codex_home.as_deref() != Some(self.startup.home()) {
            return self.fail();
        }
        let Some(agent) = body.user_agent.as_deref() else {
            return self.fail();
        };
        if let Err(unsupported) = super::version::check_user_agent(agent) {
            self.refusal = Some(unsupported.to_string());
            return self.fail();
        }
        self.phase = Phase::OpeningThread;
        let (method, thread) = match &self.startup.session {
            TurnSession::Assigned => ("thread/start", None),
            TurnSession::Resume(id) => ("thread/resume", Some(id.as_str())),
            TurnSession::First(_) => return self.fail(),
        };
        let mut params =
            json!({"cwd":self.startup.cwd,"approvalPolicy":"never","sandbox":"workspace-write"});
        if let Some(thread) = thread {
            params["threadId"] = thread.into();
            params["excludeTurns"] = true.into();
        }
        if let Some(model) = self.startup.selection.selector() {
            params["model"] = model.into();
        }
        let Some(request) = frame(&json!({"id":2,"method":method,"params":params}), false) else {
            return self.fail();
        };
        LineRead {
            actions: vec![
                ProtocolAction::Write(
                    frame(&json!({"method":"initialized"}), false).expect("fixed frame"),
                ),
                ProtocolAction::Write(request),
            ],
            ..LineRead::default()
        }
    }

    /// Report a server the first time the turn names it, from a startup report or a tool call.
    /// The supervisor compares it with the agent's declared set.
    fn server_named(&mut self, server: &str) -> LineRead {
        if server.is_empty() || !self.servers.insert(server.to_owned()) {
            return LineRead::default();
        }
        LineRead {
            session: Some(SessionObservation {
                servers: vec![server.to_owned()],
                ..SessionObservation::default()
            }),
            ..LineRead::default()
        }
    }

    /// `mcpServer/startupStatus/updated` names every MCP server Codex starts, its own included,
    /// whatever its status.
    fn server_started(&mut self, text: &str) -> LineRead {
        let name = serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|frame| frame["params"]["name"].as_str().map(str::to_owned));
        let Some(name) = name else {
            self.unreadable += 1;
            return LineRead::default();
        };
        self.server_named(&name)
    }

    fn posture(&self, body: &wire::ThreadResponse) -> bool {
        body.cwd.as_deref() == Some(self.startup.cwd())
            && body.approval_policy.as_deref() == Some("never")
            && body.sandbox.as_ref().is_some_and(|sandbox| {
                sandbox.kind.as_deref() == Some("workspaceWrite")
                    && sandbox.network_access == Some(false)
                    && sandbox.exclude_tmpdir_env_var == Some(true)
                    && sandbox.exclude_slash_tmp == Some(true)
                    && sandbox.writable_roots.as_deref()
                        == Some(self.startup.writable_roots().as_slice())
            })
    }

    fn attributed(&mut self, thread: Option<&str>, turn: Option<&str>) -> bool {
        let yes = self.phase == Phase::Active
            && thread.is_some_and(|id| {
                self.thread
                    .as_ref()
                    .is_some_and(|thread| thread.as_str() == id)
            })
            && turn.is_some_and(|id| self.turn.as_deref() == Some(id));
        if !yes {
            self.refused += 1;
        }
        yes
    }

    fn event(&mut self, event: Event) -> LineRead {
        match event {
            Event::Item { completed, body } => {
                if !self.attributed(body.thread_id.as_deref(), body.turn_id.as_deref()) {
                    return LineRead::default();
                }
                let Some(item) = body.item else {
                    self.unreadable += 1;
                    return LineRead::default();
                };
                if item.kind.as_deref() == Some("mcpToolCall") {
                    return item
                        .server
                        .map_or_else(LineRead::default, |server| self.server_named(&server));
                }
                if item.kind.as_deref() != Some("agentMessage") {
                    return LineRead::default();
                }
                let Some(id) = item.id.filter(|id| valid_id(id)) else {
                    self.unreadable += 1;
                    return LineRead::default();
                };
                if !self.items.contains_key(&id) && self.items.len() >= MAX_ITEMS {
                    return self.fail();
                }
                let progress = self.items.entry(id).or_default();
                if !completed {
                    progress.start(0, true);
                    return LineRead::default();
                }
                let Some(text) = item.text else {
                    self.unreadable += 1;
                    return LineRead::default();
                };
                if !text.is_empty() {
                    self.final_reply = Some(FinalReply::new(&text));
                }
                let fragment = progress
                    .complete(0, &text)
                    .map(|fragment| self.layout.render(fragment));
                self.text(fragment)
            }
            Event::Delta(body) => {
                if !self.attributed(body.thread_id.as_deref(), body.turn_id.as_deref()) {
                    return LineRead::default();
                }
                let fragment = body
                    .item_id
                    .as_ref()
                    .and_then(|id| self.items.get_mut(id))
                    .zip(body.delta.as_deref())
                    .and_then(|(progress, text)| progress.delta(0, text))
                    .map(|fragment| self.layout.render(fragment));
                self.text(fragment)
            }
            Event::Usage(body) => {
                if !self.attributed(body.thread_id.as_deref(), body.turn_id.as_deref()) {
                    return LineRead::default();
                }
                self.usage = body
                    .token_usage
                    .and_then(|usage| usage.total)
                    .and_then(|total| tokens(&total));
                LineRead::default()
            }
            Event::End(body) => {
                let Some(turn) = body.turn else {
                    self.unreadable += 1;
                    return LineRead::default();
                };
                if !self.attributed(body.thread_id.as_deref(), turn.id.as_deref()) {
                    return LineRead::default();
                }
                if turn.status.as_deref() == Some("completed") && turn.error.is_none() {
                    let mut read = LineRead::default();
                    for item in turn.items.unwrap_or_default() {
                        merge(
                            &mut read,
                            self.event(Event::Item {
                                completed: true,
                                body: wire::ItemNotification {
                                    thread_id: body.thread_id.clone(),
                                    turn_id: turn.id.clone(),
                                    item: Some(item),
                                },
                            }),
                        );
                        if self.phase == Phase::Failed {
                            return read;
                        }
                    }
                    self.phase = Phase::Completed;
                    read.actions.push(ProtocolAction::Close);
                    read
                } else {
                    self.fail()
                }
            }
        }
    }

    fn text(&self, fragment: Option<String>) -> LineRead {
        LineRead {
            text: fragment
                .zip(self.thread.clone())
                .map(|(text, session)| TextBlock { session, text })
                .into_iter()
                .collect(),
            ..LineRead::default()
        }
    }
}

impl TurnReader for AppServerReader {
    fn start(&mut self) -> Vec<ProtocolAction> {
        if self.phase != Phase::New {
            return self.fail().actions;
        }
        self.phase = Phase::Initializing;
        vec![ProtocolAction::Write(CodexStartup::initialize())]
    }

    fn line(&mut self, text: &str) -> LineRead {
        if matches!(self.phase, Phase::Completed | Phase::Failed) {
            return LineRead::default();
        }
        let Ok(envelope) = serde_json::from_str::<wire::Envelope>(text) else {
            self.unreadable += 1;
            return LineRead::default();
        };
        if let Some(id) = envelope.id {
            if envelope.method.is_some() {
                return self.fail();
            }
            if envelope.error.is_some() {
                return self.fail();
            }
            return id
                .as_u64()
                .map_or_else(LineRead::default, |id| self.response(id, text));
        }
        let event = match envelope.method.as_deref() {
            Some("item/started") => notification(text).map(|body| Event::Item {
                completed: false,
                body,
            }),
            Some("item/completed") => notification(text).map(|body| Event::Item {
                completed: true,
                body,
            }),
            Some("item/agentMessage/delta") => notification(text).map(Event::Delta),
            Some("thread/tokenUsage/updated") => notification(text).map(Event::Usage),
            Some("turn/completed") => notification(text).map(Event::End),
            Some("mcpServer/startupStatus/updated") => return self.server_started(text),
            _ => return LineRead::default(),
        };
        let Some(event) = event else {
            self.unreadable += 1;
            return LineRead::default();
        };
        if self.phase == Phase::StartingTurn {
            if self.early.len() >= MAX_EARLY_EVENTS
                || self.early_bytes.saturating_add(text.len()) > MAX_EARLY_BYTES
            {
                return self.fail();
            }
            self.early_bytes += text.len();
            self.early.push(event);
            return LineRead::default();
        }
        self.event(event)
    }

    fn end(self: Box<Self>, exit: ExitStatus) -> TurnEnd {
        let outcome = match self.phase {
            Phase::Completed if exit.is_success() => TurnOutcome::Completed,
            Phase::Completed => TurnOutcome::Failed {
                reason: exit_name(exit),
            },
            Phase::Failed => TurnOutcome::Failed {
                reason: self
                    .refusal
                    .unwrap_or_else(|| "Codex streaming protocol failed".into()),
            },
            _ => TurnOutcome::NoResult,
        };
        let final_reply = self
            .final_reply
            .filter(|_| outcome == TurnOutcome::Completed);
        TurnEnd {
            report: dudes_proto::TurnReport::default(),
            final_observation: None,
            last_message: final_reply.as_ref().map(|reply| reply.text.clone()),
            final_reply,
            outcome,
            usage: self.usage.map(|total| ScopedUsage::Session {
                total,
                calls: Usage::ZERO,
            }),
            model_usage: BTreeMap::new(),
            usage_refused: None,
            session: self.turn.and(self.thread),
            attribution_refused: self.refused,
            unreadable: self.unreadable,
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
}
fn frame(value: &serde_json::Value, task: bool) -> Option<StartupFrame> {
    StartupFrame::new(serde_json::to_vec(value).ok()?, task)
}
fn response<T: DeserializeOwned + Default>(text: &str) -> Option<T> {
    serde_json::from_str::<wire::Response<T>>(text).ok()?.result
}
fn notification<T: DeserializeOwned + Default>(text: &str) -> Option<T> {
    serde_json::from_str::<wire::Notification<T>>(text)
        .ok()?
        .params
}
fn tokens(total: &wire::TokenTotals) -> Option<Usage> {
    let (input, cached, output) = (
        total.input_tokens?,
        total.cached_input_tokens?,
        total.output_tokens?,
    );
    if input == 0 && cached == 0 && output == 0 {
        return None;
    }
    Some(Usage {
        input: Tokens::new(
            input
                .saturating_sub(cached)
                .saturating_sub(total.cache_write_input_tokens.unwrap_or(0)),
        ),
        cache_creation: Tokens::new(total.cache_write_input_tokens.unwrap_or(0)),
        output: Tokens::new(output),
        cache_read: Tokens::new(cached),
        ..Usage::ZERO
    })
}
fn merge(into: &mut LineRead, from: LineRead) {
    into.text.extend(from.text);
    into.actions.extend(from.actions);
    into.omitted_text = from.omitted_text.or(into.omitted_text.take());
    into.activity = from.activity.or(into.activity);
    match (&mut into.session, from.session) {
        (Some(observed), Some(more)) => {
            observed.servers.extend(more.servers);
            observed.session = observed.session.take().or(more.session);
        }
        (session, more) => *session = session.take().or(more),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::TurnSession;
    use crate::protocol::ProtocolAction;
    use dudes_types::ModelSelection;
    use serde_json::{Value, json};

    const THREAD: &str = "01a0e1d9-3860-7800-a9ff-2994ef3b1c6f";
    const TURN: &str = "turn-1";
    const EXIT_0: ExitStatus = ExitStatus {
        code: Some(0),
        signal: None,
    };

    fn reader() -> AppServerReader {
        AppServerReader::new(CodexStartup::new(
            "/data/providers/codex".into(),
            "/data/agents/a".into(),
            "/repo".into(),
            "the immutable task".into(),
            TurnSession::Assigned,
            ModelSelection::provider_default(),
        ))
    }
    #[expect(
        clippy::needless_pass_by_value,
        reason = "each test fixture is consumed as one stream event"
    )]
    fn read(reader: &mut AppServerReader, value: Value) -> LineRead {
        reader.line(&value.to_string())
    }
    fn thread_response() -> Value {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let roots = json!(["/data/agents/a/tmp", "/data/agents/a/outbox"]);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let roots = json!(["/data/agents/a/tmp"]);
        json!({"id":2,"result":{"thread":{"id":THREAD,"status":{"type":"idle"}},"cwd":"/repo","approvalPolicy":"never","sandbox":{"type":"workspaceWrite","writableRoots":roots,"networkAccess":false,"excludeTmpdirEnvVar":true,"excludeSlashTmp":true}}})
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn fresh_and_resumed_tasks_require_exact_temporary_and_own_outbox_roots() {
        for session in [
            TurnSession::Assigned,
            TurnSession::Resume(SessionId::new(THREAD)),
        ] {
            let mut reader = AppServerReader::new(CodexStartup::new(
                "/data/providers/codex".into(),
                "/data/agents/a".into(),
                "/repo".into(),
                "the immutable task".into(),
                session,
                ModelSelection::provider_default(),
            ));
            reader.start();
            read(
                &mut reader,
                json!({"id":1,"result":{"userAgent":"dudes/0.160.1 (test)","codexHome":"/data/providers/codex"}}),
            );
            let mut response = thread_response();
            response["result"]["sandbox"]["writableRoots"] =
                json!(["/data/agents/a/tmp", "/data/agents/a/outbox"]);
            let result = read(&mut reader, response);
            let [ProtocolAction::Write(frame)] = result.actions.as_slice() else {
                panic!("the exact admitted roots must submit the task");
            };
            let task: Value = serde_json::from_slice(frame.bytes()).unwrap();
            assert_eq!(
                task["params"]["sandboxPolicy"],
                json!({
                    "type":"workspaceWrite", "writableRoots":["/data/agents/a/tmp","/data/agents/a/outbox"],
                    "networkAccess":false, "excludeTmpdirEnvVar":true, "excludeSlashTmp":true
                })
            );
            assert_eq!(task["params"]["approvalPolicy"], "never");
            assert_eq!(task["params"]["cwd"], "/repo");
        }
    }
    fn ready(reader: &mut AppServerReader) {
        assert_eq!(reader.start().len(), 1);
        let initialized = read(
            reader,
            json!({"id":1,"result":{"userAgent":"dudes/0.160.0 (test)","codexHome":"/data/providers/codex"}}),
        );
        assert_eq!(initialized.actions.len(), 2);
        let thread = read(reader, thread_response());
        assert!(
            matches!(thread.actions.as_slice(),[ProtocolAction::Write(frame)] if frame.is_task())
        );
        let turn = read(
            reader,
            json!({"id":3,"result":{"turn":{"id":TURN,"status":"inProgress"}}}),
        );
        assert_eq!(turn.session.unwrap().session.unwrap().as_str(), THREAD);
        assert_eq!(turn.actions, [ProtocolAction::Started]);
    }
    fn item(method: &str, id: &str, text: Option<&str>) -> Value {
        json!({"method":method,"params":{"threadId":THREAD,"turnId":TURN,"item":{"type":"agentMessage","id":id,"text":text}}})
    }
    fn delta(id: &str, text: &str) -> Value {
        json!({"method":"item/agentMessage/delta","params":{"threadId":THREAD,"turnId":TURN,"itemId":id,"delta":text}})
    }
    fn complete() -> Value {
        json!({"method":"turn/completed","params":{"threadId":THREAD,"turn":{"id":TURN,"status":"completed"}}})
    }
    #[test]
    fn fragments_arrive_before_completion_and_full_item_is_not_duplicated() {
        let mut reader = reader();
        ready(&mut reader);
        assert!(
            read(&mut reader, item("item/started", "a", None))
                .text
                .is_empty()
        );
        assert_eq!(read(&mut reader, delta("a", "hel")).text[0].text, "hel");
        assert_eq!(read(&mut reader, delta("a", "lo")).text[0].text, "lo");
        assert!(
            read(&mut reader, item("item/completed", "a", Some("hello")))
                .text
                .is_empty()
        );
        assert_eq!(
            read(&mut reader, complete()).actions,
            [ProtocolAction::Close]
        );
        let end = Box::new(reader).end(EXIT_0);
        assert_eq!(end.outcome, dudes_proto::TurnOutcome::Completed);
        assert_eq!(end.final_reply.unwrap().text, "hello");
    }
    #[test]
    fn early_events_wait_for_matching_turn_response_and_other_turns_are_dropped() {
        let mut reader = reader();
        reader.start();
        read(
            &mut reader,
            json!({"id":1,"result":{"userAgent":"dudes/0.160.0 (test)","codexHome":"/data/providers/codex"}}),
        );
        read(&mut reader, thread_response());
        read(&mut reader, item("item/started", "a", None));
        let mut foreign = delta("a", "foreign");
        foreign["params"]["turnId"] = "old-turn".into();
        assert!(read(&mut reader, foreign).text.is_empty());
        assert!(read(&mut reader, delta("a", "live")).text.is_empty());
        let bound = read(
            &mut reader,
            json!({"id":3,"result":{"turn":{"id":TURN,"status":"inProgress"}}}),
        );
        assert_eq!(
            bound
                .text
                .iter()
                .map(|block| block.text.as_str())
                .collect::<String>(),
            "live"
        );
        assert_eq!(reader.refused, 1);
    }

    #[test]
    fn hidden_unstarted_foreign_and_malformed_fragments_cannot_replace_the_final_reply() {
        let mut reader = reader();
        ready(&mut reader);
        let mut hidden = item("item/started", "secret", None);
        hidden["params"]["item"]["type"] = "reasoning".into();
        read(&mut reader, hidden);
        assert!(read(&mut reader, delta("secret", "hidden")).text.is_empty());
        assert!(
            read(&mut reader, delta("unknown", "unstarted"))
                .text
                .is_empty()
        );
        read(&mut reader, item("item/started", "a", None));
        let mut foreign = delta("a", "foreign");
        foreign["params"]["threadId"] = "another-thread".into();
        assert!(read(&mut reader, foreign).text.is_empty());
        let mut malformed = delta("a", "bad");
        malformed["params"]["delta"] = 17.into();
        assert!(read(&mut reader, malformed).text.is_empty());
        assert_eq!(
            read(&mut reader, item("item/completed", "a", Some("correct"))).text[0].text,
            "correct"
        );
        read(&mut reader, complete());
        let end = Box::new(reader).end(EXIT_0);
        assert_eq!(end.final_reply.unwrap().text, "correct");
        assert_eq!(end.unreadable, 1);
        assert_eq!(end.attribution_refused, 1);
    }

    #[test]
    fn logical_items_have_one_separator_and_fallback_completions_are_exact() {
        let mut reader = reader();
        ready(&mut reader);
        let mut output = String::new();
        for event in [
            item("item/started", "empty", None),
            item("item/completed", "empty", Some("")),
            item("item/started", "a", None),
            delta("a", "```"),
            delta("a", "code```"),
            item("item/completed", "a", Some("```code```")),
            item("item/completed", "b", Some("done")),
            item("item/completed", "b", Some("done")),
        ] {
            for block in read(&mut reader, event).text {
                output.push_str(&block.text);
            }
        }
        assert_eq!(output, "```code```\ndone");
    }

    #[test]
    fn cumulative_usage_excludes_replayed_turn_usage_and_last_request_usage() {
        let mut reader = reader();
        ready(&mut reader);
        let usage = json!({"method":"thread/tokenUsage/updated","params":{"threadId":THREAD,"turnId":TURN,"tokenUsage":{"total":{"inputTokens":120,"cachedInputTokens":20,"outputTokens":9},"last":{"inputTokens":1,"cachedInputTokens":0,"outputTokens":1}}}});
        read(&mut reader, usage.clone());
        let mut old = usage;
        old["params"]["turnId"] = "old".into();
        old["params"]["tokenUsage"]["total"]["outputTokens"] = 999.into();
        read(&mut reader, old);
        read(&mut reader, complete());
        let end = Box::new(reader).end(EXIT_0);
        assert!(
            matches!(end.usage,Some(ScopedUsage::Session{total,..}) if total.input==Tokens::new(100) && total.output==Tokens::new(9) && total.cache_read==Tokens::new(20))
        );
    }

    #[test]
    fn final_success_requires_terminal_notification_and_real_successful_exit() {
        let mut reader = reader();
        ready(&mut reader);
        read(&mut reader, item("item/completed", "a", Some("done")));
        let end = Box::new(reader).end(EXIT_0);
        assert_eq!(end.outcome, TurnOutcome::NoResult);
        assert!(end.final_reply.is_none());
        let mut reader = super::tests::reader();
        ready(&mut reader);
        read(&mut reader, item("item/completed", "a", Some("done")));
        read(&mut reader, complete());
        let end = Box::new(reader).end(ExitStatus {
            code: Some(1),
            signal: None,
        });
        assert!(matches!(end.outcome, TurnOutcome::Failed { .. }));
        assert!(end.final_reply.is_none());
    }

    #[test]
    fn altered_posture_and_wrong_home_never_submit_a_task() {
        for field in ["approval", "roots", "tmp", "network"] {
            let mut reader = reader();
            reader.start();
            read(
                &mut reader,
                json!({"id":1,"result":{"userAgent":"dudes/0.160.0 (test)","codexHome":"/data/providers/codex"}}),
            );
            let mut response = thread_response();
            match field {
                "approval" => response["result"]["approvalPolicy"] = "on-request".into(),
                "roots" => response["result"]["sandbox"]["writableRoots"] = json!(["/"]),
                "tmp" => response["result"]["sandbox"]["excludeSlashTmp"] = false.into(),
                _ => response["result"]["sandbox"]["networkAccess"] = true.into(),
            }
            assert_eq!(read(&mut reader, response).actions, [ProtocolAction::Fail]);
        }
        let mut reader = reader();
        reader.start();
        assert_eq!(
            read(
                &mut reader,
                json!({"id":1,"result":{"userAgent":"dudes/0.160.0 (test)","codexHome":"/other"}})
            )
            .actions,
            [ProtocolAction::Fail]
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn missing_extra_reordered_and_other_agent_roots_never_submit_a_task() {
        for roots in [
            json!(["/data/agents/a/tmp"]),
            json!(["/data/agents/a/tmp", "/data/agents/a/outbox", "/"]),
            json!(["/data/agents/a/outbox", "/data/agents/a/tmp"]),
            json!(["/data/agents/a/tmp", "/data/agents/b/outbox"]),
            json!(["/data/agents/a/tmp", "/data/agents/a/inbox"]),
            json!(["/data/agents/a/tmp", "/data/agents/a/../a/outbox"]),
        ] {
            let mut reader = reader();
            reader.start();
            read(
                &mut reader,
                json!({"id":1,"result":{"userAgent":"dudes/0.160.1 (test)","codexHome":"/data/providers/codex"}}),
            );
            let mut response = thread_response();
            response["result"]["sandbox"]["writableRoots"] = roots;
            assert_eq!(read(&mut reader, response).actions, [ProtocolAction::Fail]);
        }
    }

    #[test]
    fn unrequested_rpc_and_bounded_early_buffer_fail_closed() {
        let mut reader = reader();
        reader.start();
        assert_eq!(
            read(
                &mut reader,
                json!({"id":700,"method":"item/commandExecution/requestApproval","params":{}})
            )
            .actions,
            [ProtocolAction::Fail]
        );
        let mut reader = super::tests::reader();
        reader.start();
        read(
            &mut reader,
            json!({"id":1,"result":{"userAgent":"dudes/0.160.0 (test)","codexHome":"/data/providers/codex"}}),
        );
        read(&mut reader, thread_response());
        for _ in 0..MAX_EARLY_EVENTS {
            assert!(read(&mut reader, delta("a", "small")).actions.is_empty());
        }
        assert_eq!(
            read(&mut reader, delta("a", "excess")).actions,
            [ProtocolAction::Fail]
        );
        assert!(reader.early.is_empty());
    }

    #[test]
    fn bounded_visible_item_bookkeeping_does_not_count_hidden_items() {
        let mut reader = reader();
        ready(&mut reader);
        for index in 0..(MAX_ITEMS * 2) {
            let mut hidden = item("item/started", &format!("hidden-{index}"), None);
            hidden["params"]["item"]["type"] = "reasoning".into();
            read(&mut reader, hidden);
        }
        assert!(reader.items.is_empty());
        for index in 0..MAX_ITEMS {
            read(
                &mut reader,
                item("item/started", &format!("a-{index}"), None),
            );
        }
        assert_eq!(
            read(&mut reader, item("item/started", "excess", None)).actions,
            [ProtocolAction::Fail]
        );
        assert_eq!(reader.items.len(), MAX_ITEMS);
    }
    #[test]
    fn a_patch_release_of_the_supported_line_opens_the_thread() {
        let mut reader = reader();
        reader.start();
        let opened = read(
            &mut reader,
            json!({"id":1,"result":{"userAgent":"dudes/0.160.1 (Mac OS 14.1.1; arm64) WarpTerminal","codexHome":"/data/providers/codex"}}),
        );
        let methods: Vec<Value> = opened
            .actions
            .iter()
            .map(|action| match action {
                ProtocolAction::Write(frame) => {
                    serde_json::from_slice::<Value>(frame.bytes()).unwrap()["method"].clone()
                }
                other => json!(format!("{other:?}")),
            })
            .collect();
        assert_eq!(methods, [json!("initialized"), json!("thread/start")]);
    }

    #[test]
    fn another_minor_fails_the_turn_naming_both_versions() {
        let mut reader = reader();
        reader.start();
        let refused = read(
            &mut reader,
            json!({"id":1,"result":{"userAgent":"dudes/0.161.0 (Mac OS 14.1.1; arm64)","codexHome":"/data/providers/codex"}}),
        );
        assert_eq!(refused.actions, [ProtocolAction::Fail]);
        assert_eq!(
            Box::new(reader).end(EXIT_0).outcome,
            TurnOutcome::Failed {
                reason: "Codex streaming requires codex-cli 0.160.x, but the installed Codex reports dudes/0.161.0".into()
            }
        );
    }

    fn servers(read: &LineRead) -> Option<Vec<String>> {
        read.session
            .as_ref()
            .map(|observed| observed.servers.clone())
    }

    /// Every MCP server Codex reports starting, and every server a tool call names, reaches the
    /// supervisor once per turn, which compares it with the agent's declared set; the turn goes on.
    /// Mutants: fail the turn on any server, as before servers could be declared; report a server
    /// on every status update, which would raise one rung per update; or drop a server that rode
    /// in on a buffered early event.
    #[test]
    fn each_mcp_server_codex_names_is_reported_once_and_the_turn_goes_on() {
        let status = |name: &str, status: &str| json!({"method":"mcpServer/startupStatus/updated","params":{"threadId":THREAD,"name":name,"status":status,"error":null,"failureReason":null}});
        let call = |server: &str| json!({"method":"item/started","params":{"threadId":THREAD,"turnId":TURN,"item":{"type":"mcpToolCall","id":"call-1","server":server,"tool":"echo","status":"inProgress"}}});
        let mut reader = reader();
        reader.start();
        read(
            &mut reader,
            json!({"id":1,"result":{"userAgent":"dudes/0.160.1 (test)","codexHome":"/data/providers/codex"}}),
        );
        let started = read(&mut reader, status("memory", "starting"));
        assert_eq!(servers(&started), Some(vec!["memory".to_owned()]));
        assert!(started.actions.is_empty());
        assert_eq!(servers(&read(&mut reader, status("memory", "ready"))), None);
        assert_eq!(read(&mut reader, thread_response()).actions.len(), 1);
        assert_eq!(
            servers(&read(&mut reader, call("search"))),
            None,
            "buffered"
        );
        let turn = read(
            &mut reader,
            json!({"id":3,"result":{"turn":{"id":TURN,"status":"inProgress"}}}),
        );
        let observed = turn.session.expect("the turn opens its thread");
        assert_eq!(
            observed.session.map(|id| id.as_str().to_owned()),
            Some(THREAD.to_owned())
        );
        assert_eq!(observed.servers, ["search"]);
        assert_eq!(servers(&read(&mut reader, call("search"))), None);
        assert_eq!(
            servers(&read(&mut reader, status("codex_apps", "failed"))),
            Some(vec!["codex_apps".to_owned()])
        );
        assert_eq!(
            servers(&read(&mut reader, call("rogue"))),
            Some(vec!["rogue".to_owned()])
        );
        read(&mut reader, item("item/completed", "msg", Some("done")));
        assert_eq!(
            read(&mut reader, complete()).actions,
            [ProtocolAction::Close]
        );
        assert_eq!(Box::new(reader).end(EXIT_0).outcome, TurnOutcome::Completed);
    }

    #[test]
    fn only_a_known_idle_thread_can_receive_a_new_task() {
        for resume in [false, true] {
            for status in [
                None,
                Some("active"),
                Some("notLoaded"),
                Some("systemError"),
                Some("futureStatus"),
            ] {
                let mut reader = reader();
                if resume {
                    reader.startup.session = TurnSession::Resume(SessionId::new(THREAD));
                }
                reader.start();
                read(
                    &mut reader,
                    json!({"id":1,"result":{"userAgent":"dudes/0.160.0 (test)","codexHome":"/data/providers/codex"}}),
                );
                let mut response = thread_response();
                response["result"]["thread"]["status"] =
                    status.map_or(Value::Null, |kind| json!({"type":kind}));
                assert_eq!(read(&mut reader, response).actions, [ProtocolAction::Fail]);
            }
        }
    }
    #[test]
    fn unavailable_resume_is_a_failed_unconfirmed_turn_without_task_delivery() {
        let mut reader = reader();
        reader.startup.session = TurnSession::Resume(SessionId::new(THREAD));
        reader.start();
        let opened = read(
            &mut reader,
            json!({"id":1,"result":{"userAgent":"dudes/0.160.0 (test)","codexHome":"/data/providers/codex"}}),
        );
        assert!(
            opened
                .actions
                .iter()
                .all(|action| matches!(action,ProtocolAction::Write(frame) if !frame.is_task()))
        );
        let ProtocolAction::Write(frame) = &opened.actions[1] else {
            panic!("thread request")
        };
        let request: Value = serde_json::from_slice(frame.bytes()).unwrap();
        assert_eq!(request["method"], "thread/resume");
        assert_eq!(request["params"]["threadId"], THREAD);
        let refused = read(
            &mut reader,
            json!({"id":2,"error":{"code":-32600,"message":"thread not found"}}),
        );
        assert_eq!(refused.actions, [ProtocolAction::Fail]);
        assert!(refused.session.is_none());
        let end = Box::new(reader).end(EXIT_0);
        assert!(matches!(end.outcome, TurnOutcome::Failed { .. }));
        assert!(end.session.is_none());
        assert!(end.final_reply.is_none());
    }
}
