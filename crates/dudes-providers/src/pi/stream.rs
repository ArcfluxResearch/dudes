//! One `pi --mode json` run's standard output, read into what the turn's end carries.
//!
//! # The turn ends at `agent_settled`
//!
//! `agent_end` closes one low-level agent run, and Pi may start another after it: an automatic
//! retry, a compaction retry or a queued continuation. `agent_settled` says that none remains. The
//! turn therefore ends at the first `agent_settled`, and the reader ignores any line after it. A
//! stream that ends without one has no result, and its exit status is all there is.
//!
//! # Usage is one figure per provider call, added once
//!
//! Each assistant message is one provider call of the agent loop. Every `message_update` restates
//! the usage the provider has reported for the message so far, which stays at zero with a provider
//! that reports only at the end, and `message_end` carries the final figure. The reader keeps the
//! latest figure of the message in flight and adds it to the turn once, when the message ends.
//! Automatic compaction summarises the conversation through provider calls of its own, outside the
//! agent loop. No message event carries them, so the reader adds `compaction_end`'s `result.usage`.
//! The figures are this turn's own, so the reader reports them as [`ScopedUsage::Turn`]. A resumed
//! turn reports only its own calls, and their input counts the history Pi sent again. Pi's `cost`
//! is not charged, because Pi computes it from a local price catalog.
//!
//! Pi starts every assistant message at zero and fills in what the provider reports, so zero means
//! that nothing was reported. A turn whose messages all stayed at zero has no usage.
//! Neither does a turn with a call whose figure never arrived: a message cut off while still at
//! zero, a compaction that started and never ended, or one that ended without a result. Charging
//! the turn's other calls alone would under-report it with no sign that anything is missing.
//!
//! # The outcome comes from the final assistant message
//!
//! JSON mode exits 0 even when the final call failed, so the exit status alone cannot show a
//! failure. A final `stopReason` of `stop` with exit 0 completes the turn. Any other `stopReason`
//! fails it, with Pi's `errorMessage` when there is one, and so does a non-zero exit.
//!
//! # The session id must fit Pi's own grammar
//!
//! Pi accepts a session id made of ASCII letters, digits, `.`, `_` and `-` that starts and ends with
//! a letter or digit (`assertValidSessionId` in its `core/session-manager.js`). The reader holds the
//! header's id to the same grammar, and also refuses an id ending in `.jsonl`, because `--session`
//! reads such an argument as a file path. A refused id gives a session observation with no session.

use std::collections::{BTreeMap, BTreeSet};

use dudes_proto::providers::pi::{
    AssistantMessage, CompactionEnd, MessageEvent, MessageUpdate, SessionHeader,
};
use dudes_proto::{ExitStatus, LiveActivityKind, TurnOutcome};
use dudes_types::{SessionId, Tokens, Usage};
use serde::Deserialize;
use serde_json::Value;

use crate::stream::{
    FinalReply, LineRead, ScopedUsage, SessionObservation, TextBlock, TextFragment, TextLayout,
    TextProgress, TurnEnd, TurnReader, exit_name,
};

/// One `pi --mode json` run's standard output, as far as it has been read.
#[derive(Debug, Default)]
pub struct TurnStream {
    phase: Phase,
    session: Option<SessionId>,
    in_flight: Option<Call>,
    usage: Option<Usage>,
    model_usage: BTreeMap<String, Usage>,
    last: Option<AssistantMessage>,
    compacting: bool,
    unreported: bool,
    unreadable: u64,
    layout: TextLayout,
    pending_tools: BTreeMap<String, PendingTool>,
    completed_servers: BTreeSet<String>,
}

#[derive(Debug)]
struct PendingTool {
    name: String,
    server: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    BeforeHeader,
    Turn,
    /// `agent_settled` has arrived, and a later line belongs to no turn.
    Settled,
}

/// The assistant message in flight. `usage` is its latest figure, not a sum.
#[derive(Debug, Default)]
struct Call {
    model: Option<String>,
    usage: Usage,
    text: Option<TextProgress>,
}

enum Event {
    Session(SessionHeader),
    Start(AssistantMessage),
    Update(MessageUpdate, Option<ContentEvent>),
    End(AssistantMessage),
    CompactionStart,
    CompactionEnd(CompactionEnd),
    ToolStart {
        id: Option<String>,
        name: Option<String>,
        server: Option<String>,
    },
    ToolEnd {
        id: Option<String>,
        name: Option<String>,
        succeeded: bool,
    },
    Settled,
    RunEnd,
    Other,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ContentEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "contentIndex")]
    index: Option<usize>,
    delta: Option<String>,
    content: Option<String>,
}

impl ContentEvent {
    fn read<'a>(&'a self, progress: &mut TextProgress) -> Option<TextFragment<'a>> {
        let index = self.index?;
        match self.kind.as_str() {
            "text_start" | "thinking_start" | "toolcall_start" => {
                progress.start(index, self.kind == "text_start");
                None
            }
            "text_delta" => progress.delta(index, self.delta.as_deref()?),
            "text_end" => progress.complete(index, self.content.as_deref()?),
            _ => None,
        }
    }
}

impl Event {
    fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let string = |key| {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let assistant = value.pointer("/message/role").and_then(Value::as_str) == Some("assistant");
        Some(match value.get("type")?.as_str()? {
            "session" => Self::Session(SessionHeader::deserialize(&value).ok()?),
            "message_start" if assistant => {
                Self::Start(MessageEvent::deserialize(&value).ok()?.message)
            }
            "message_end" if assistant => {
                Self::End(MessageEvent::deserialize(&value).ok()?.message)
            }
            "message_update" => Self::Update(
                MessageUpdate::deserialize(&value).ok()?,
                value
                    .get("assistantMessageEvent")
                    .and_then(|event| ContentEvent::deserialize(event).ok()),
            ),
            "compaction_start" => Self::CompactionStart,
            "compaction_end" => Self::CompactionEnd(CompactionEnd::deserialize(&value).ok()?),
            "tool_execution_start" => {
                let name = string("toolName");
                let server = name
                    .as_deref()
                    .and_then(|name| {
                        if let Some(tool) = name.strip_prefix("mcp__") {
                            return tool
                                .split_once("__")
                                .map(|(server, _)| server)
                                .filter(|server| !server.is_empty());
                        }
                        matches!(
                            name,
                            "list_mcp_resources"
                                | "list_mcp_resource_templates"
                                | "read_mcp_resource"
                        )
                        .then(|| value.pointer("/args/server").and_then(Value::as_str))
                        .flatten()
                        .filter(|server| !server.is_empty())
                    })
                    .map(str::to_owned);
                Self::ToolStart {
                    id: string("toolCallId"),
                    name,
                    server,
                }
            }
            "tool_execution_end" => Self::ToolEnd {
                id: string("toolCallId"),
                name: string("toolName"),
                succeeded: value.get("isError").and_then(Value::as_bool) == Some(false),
            },
            "agent_settled" => Self::Settled,
            "agent_end" | "auto_retry_start" => Self::RunEnd,
            _ => Self::Other,
        })
    }
}

impl TurnStream {
    /// An empty reader, before the child has written anything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn complete_tool(&mut self, id: Option<&str>, name: Option<&str>, succeeded: bool) {
        if let Some(pending) = id.and_then(|id| self.pending_tools.remove(id))
            && succeeded
            && name == Some(pending.name.as_str())
        {
            self.completed_servers.insert(pending.server);
        }
    }

    fn update_message(
        &mut self,
        update: &MessageUpdate,
        content: Option<&ContentEvent>,
    ) -> LineRead {
        if let Some(figure) = update.usage {
            self.in_flight.get_or_insert_default().usage = usage(figure);
        }
        let text = self
            .in_flight
            .as_mut()
            .and_then(|call| call.text.as_mut())
            .zip(content)
            .and_then(|(progress, event)| event.read(progress))
            .zip(self.session.as_ref())
            .map(|(text, session)| TextBlock {
                session: session.clone(),
                text: self.layout.render(text),
            });
        LineRead {
            text: text.into_iter().collect(),
            omitted_text: self
                .in_flight
                .as_mut()
                .and_then(|call| call.text.as_mut())
                .is_some_and(TextProgress::take_omitted_text)
                .then(|| self.session.clone())
                .flatten(),
            ..LineRead::default()
        }
    }

    fn complete_message(&mut self, message: AssistantMessage) -> LineRead {
        let mut call = self.in_flight.take().unwrap_or_default();
        call.model = message.model.clone().or(call.model);
        if let Some(figure) = message.usage {
            call.usage = usage(figure);
        }
        let mut progress = call.text.take().unwrap_or_default();
        let text = self.session.as_ref().map_or_else(Vec::new, |session| {
            message
                .content
                .iter()
                .enumerate()
                .filter(|(_, block)| block.kind == "text")
                .filter_map(|(index, block)| progress.complete(index, block.text.as_deref()?))
                .map(|text| TextBlock {
                    session: session.clone(),
                    text: self.layout.render(text),
                })
                .collect()
        });
        self.add(call);
        self.last = Some(message);
        LineRead {
            actions: Vec::new(),
            omitted_text: progress
                .take_omitted_text()
                .then(|| self.session.clone())
                .flatten(),
            text,
            session: None,
            activity: Some(LiveActivityKind::Working),
        }
    }

    /// A call still at zero reported nothing, so it adds nothing.
    fn add(&mut self, call: Call) {
        if call.usage.total_tokens() == Tokens::ZERO {
            return;
        }
        self.usage = Some(self.usage.unwrap_or_default().saturating_add(call.usage));
        if let Some(model) = call.model {
            let used = self.model_usage.entry(model).or_default();
            *used = used.saturating_add(call.usage);
        }
    }

    /// A call that never reached `message_end` consumed at least its last figure. At zero, what it
    /// consumed is unknown.
    fn abandon(&mut self, call: Call) {
        if call.usage.total_tokens() == Tokens::ZERO {
            self.unreported = true;
        } else {
            self.add(call);
        }
    }
}

impl TurnReader for TurnStream {
    fn line(&mut self, text: &str) -> LineRead {
        if self.phase == Phase::Settled {
            return LineRead::default();
        }
        let Some(event) = Event::parse(text) else {
            self.unreadable += 1;
            return LineRead::default();
        };
        match event {
            Event::Session(header) if self.phase == Phase::BeforeHeader => {
                self.phase = Phase::Turn;
                self.session = session_id(&header.id);
                return LineRead {
                    actions: Vec::new(),
                    omitted_text: None,
                    text: Vec::new(),
                    session: Some(SessionObservation {
                        session: self.session.clone(),
                        ..SessionObservation::default()
                    }),
                    activity: Some(LiveActivityKind::Working),
                };
            }
            Event::Start(message) => {
                if let Some(call) = self.in_flight.take() {
                    self.abandon(call);
                }
                self.in_flight = Some(Call {
                    model: message.model,
                    usage: message.usage.map(usage).unwrap_or_default(),
                    text: (self.session.is_some() && !self.compacting).then(TextProgress::default),
                });
                return LineRead {
                    activity: Some(LiveActivityKind::Working),
                    ..LineRead::default()
                };
            }
            Event::Update(update, content) => {
                return self.update_message(&update, content.as_ref());
            }
            Event::End(message) => return self.complete_message(message),
            Event::CompactionStart => {
                self.compacting = true;
                if let Some(call) = self.in_flight.as_mut() {
                    call.text = None;
                }
                return LineRead {
                    activity: Some(LiveActivityKind::Compacting),
                    ..LineRead::default()
                };
            }
            Event::CompactionEnd(end) => {
                self.compacting = false;
                match end.result {
                    Some(result) => self.add(Call {
                        model: None,
                        usage: result.usage.map(usage).unwrap_or_default(),
                        text: None,
                    }),
                    None => self.unreported = true,
                }
                return LineRead {
                    activity: Some(LiveActivityKind::Working),
                    ..LineRead::default()
                };
            }
            Event::ToolStart { id, name, server } => {
                if let Some(id) = id {
                    self.pending_tools.remove(&id);
                    if let Some((name, server)) = name.zip(server) {
                        self.pending_tools.insert(id, PendingTool { name, server });
                    }
                }
                return LineRead {
                    activity: Some(LiveActivityKind::UsingTool),
                    ..LineRead::default()
                };
            }
            Event::ToolEnd {
                id,
                name,
                succeeded,
            } => {
                self.complete_tool(id.as_deref(), name.as_deref(), succeeded);
                return LineRead {
                    activity: Some(LiveActivityKind::Working),
                    ..LineRead::default()
                };
            }
            Event::Settled => self.phase = Phase::Settled,
            Event::RunEnd => {
                if let Some(call) = self.in_flight.as_mut() {
                    call.text = None;
                }
            }
            Event::Session(_) | Event::Other => {}
        }
        LineRead::default()
    }

    fn end(self: Box<Self>, exit: ExitStatus) -> TurnEnd {
        let mut turn = *self;
        if let Some(call) = turn.in_flight.take() {
            turn.abandon(call);
        }
        turn.unreported |= turn.compacting;
        let settled = turn.phase == Phase::Settled;
        let (outcome, final_reply) = match turn.last.as_ref().filter(|_| settled) {
            Some(last) => outcome(last, exit),
            None => (TurnOutcome::NoResult, None),
        };
        let last_message = final_reply.as_ref().map(|reply| reply.text.clone());
        TurnEnd {
            final_observation: (!turn.completed_servers.is_empty()).then(|| SessionObservation {
                servers: turn.completed_servers.into_iter().collect(),
                ..SessionObservation::default()
            }),
            outcome,
            usage: turn
                .usage
                .filter(|_| !turn.unreported)
                .map(ScopedUsage::Turn),
            model_usage: turn.model_usage,
            last_message,
            final_reply,
            usage_refused: None,
            session: turn.session,
            attribution_refused: 0,
            unreadable: turn.unreadable,
        }
    }
}

fn outcome(last: &AssistantMessage, exit: ExitStatus) -> (TurnOutcome, Option<FinalReply>) {
    match last.stop_reason.as_deref() {
        Some("stop") if exit.is_success() => {
            let text = texts(last).collect::<Vec<_>>().join("\n");
            (
                TurnOutcome::Completed,
                (!text.is_empty()).then(|| FinalReply::new(&text)),
            )
        }
        Some("stop") => (
            TurnOutcome::Failed {
                reason: exit_name(exit),
            },
            None,
        ),
        other => {
            let signal = other.unwrap_or("no stop reason");
            let reason = match last.error_message.as_deref() {
                Some(message) if !message.is_empty() => format!("{signal}: {message}"),
                _ => signal.to_owned(),
            };
            (TurnOutcome::Failed { reason }, None)
        }
    }
}

fn texts(message: &AssistantMessage) -> impl Iterator<Item = &str> {
    message
        .content
        .iter()
        .filter(|block| block.kind == "text")
        .filter_map(|block| block.text.as_deref().filter(|text| !text.is_empty()))
}

fn usage(figure: dudes_proto::providers::pi::Usage) -> Usage {
    Usage {
        input: Tokens::new(figure.input),
        output: Tokens::new(figure.output),
        cache_read: Tokens::new(figure.cache_read),
        cache_creation: Tokens::new(figure.cache_write),
        usd: None,
    }
}

#[allow(clippy::case_sensitive_file_extension_comparisons)] // Pi's own test is case-sensitive
fn session_id(id: &str) -> Option<SessionId> {
    let ends = id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id.ends_with(|c: char| c.is_ascii_alphanumeric());
    let charset = id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    (ends && charset && !id.ends_with(".jsonl")).then(|| SessionId::new(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "01a0e1e4-ff40-736b-8ce4-31fdba7fd3a2";
    const EXIT_0: ExitStatus = ExitStatus {
        code: Some(0),
        signal: None,
    };

    fn header(id: &str) -> String {
        format!(r#"{{"type":"session","version":3,"id":"{id}","cwd":"/tmp/dudes-fixture"}}"#)
    }

    #[test]
    fn completed_mcp_calls_survive_abnormal_eof_and_are_reported_once() {
        let lines = [
            header(SESSION),
            r#"{"type":"tool_execution_start","toolCallId":"1","toolName":"mcp__b__x","args":{}}"#.to_owned(),
            r#"{"type":"tool_execution_end","toolCallId":"1","toolName":"mcp__b__x","isError":false}"#.to_owned(),
            r#"{"type":"tool_execution_start","toolCallId":"2","toolName":"mcp__b__x","args":{}}"#.to_owned(),
            r#"{"type":"tool_execution_end","toolCallId":"2","toolName":"mcp__b__x","isError":false}"#.to_owned(),
        ];
        let (reads, reader) = read(&lines);
        assert_eq!(
            reads.iter().filter(|read| read.session.is_some()).count(),
            1
        );
        let end = reader.end(EXIT_0);
        assert_eq!(end.outcome, TurnOutcome::NoResult);
        assert_eq!(end.final_observation.unwrap().servers, ["b"]);
    }

    #[test]
    fn resource_calls_use_paired_start_args_and_error_calls_are_not_evidence() {
        let lines = [
            header(SESSION),
            r#"{"type":"tool_execution_start","toolCallId":"resource","toolName":"read_mcp_resource","args":{"server":"b"}}"#.to_owned(),
            r#"{"type":"tool_execution_end","toolCallId":"resource","toolName":"read_mcp_resource","isError":false}"#.to_owned(),
            r#"{"type":"tool_execution_start","toolCallId":"ghost","toolName":"mcp__ghost__x"}"#.to_owned(),
            r#"{"type":"tool_execution_end","toolCallId":"ghost","toolName":"mcp__ghost__x","isError":true}"#.to_owned(),
            r#"{"type":"tool_execution_end","toolCallId":"ghost","toolName":"mcp__ghost__x","isError":false}"#.to_owned(),
            r#"{"type":"tool_execution_start","toolCallId":"missing-error","toolName":"mcp__incomplete__x"}"#.to_owned(),
            r#"{"type":"tool_execution_end","toolCallId":"missing-error","toolName":"mcp__incomplete__x"}"#.to_owned(),
            r#"{"type":"agent_settled"}"#.to_owned(),
            r#"{"type":"tool_execution_start","toolCallId":"late","toolName":"mcp__late__x"}"#.to_owned(),
            r#"{"type":"tool_execution_end","toolCallId":"late","toolName":"mcp__late__x","isError":false}"#.to_owned(),
        ];
        let (_, reader) = read(&lines);
        assert_eq!(reader.end(EXIT_0).final_observation.unwrap().servers, ["b"]);
    }

    #[test]
    fn duplicate_ids_and_malformed_pairs_cannot_reuse_an_old_start() {
        let lines = [
            header(SESSION),
            r#"{"type":"tool_execution_start","toolCallId":"duplicate","toolName":"mcp__old__x"}"#.to_owned(),
            r#"{"type":"tool_execution_start","toolCallId":"duplicate","toolName":"read"}"#.to_owned(),
            r#"{"type":"tool_execution_end","toolCallId":"duplicate","toolName":"mcp__old__x","isError":false}"#.to_owned(),
            r#"{"type":"tool_execution_start","toolCallId":"malformed","toolName":"mcp__bad__x"}"#.to_owned(),
            r#"{"type":"tool_execution_start","toolCallId":"malformed"}"#.to_owned(),
            r#"{"type":"tool_execution_end","toolCallId":"malformed","toolName":"mcp__bad__x","isError":false}"#.to_owned(),
            r#"{"type":"tool_execution_start","toolCallId":"mismatch","toolName":"mcp__bad__x"}"#.to_owned(),
            r#"{"type":"tool_execution_end","toolCallId":"mismatch","toolName":"read","isError":false}"#.to_owned(),
            r#"{"type":"tool_execution_end","toolCallId":"mismatch","toolName":"mcp__bad__x","isError":false}"#.to_owned(),
            r#"{"type":"tool_execution_end","toolCallId":"unmatched","toolName":"mcp__bad__x","isError":false}"#.to_owned(),
            r#"{"type":"tool_execution_start","toolCallId":"good","toolName":"mcp__a__x"}"#.to_owned(),
            r#"{"type":"tool_execution_end","toolCallId":"good","toolName":"mcp__a__x","isError":false}"#.to_owned(),
        ];
        let (_, reader) = read(&lines);
        assert_eq!(reader.end(EXIT_0).final_observation.unwrap().servers, ["a"]);
    }

    #[test]
    fn structured_tool_and_compaction_events_emit_fixed_activity() {
        let mut reader = TurnStream::new();
        assert_eq!(
            reader.line(&header(SESSION)).activity,
            Some(LiveActivityKind::Working)
        );
        assert_eq!(
            reader
                .line(
                    r#"{"type":"tool_execution_start","toolName":"read","args":{"path":"private"}}"#
                )
                .activity,
            Some(LiveActivityKind::UsingTool)
        );
        assert_eq!(
            reader
                .line(r#"{"type":"tool_execution_end","result":{"content":"private"}}"#)
                .activity,
            Some(LiveActivityKind::Working)
        );
        assert_eq!(
            reader
                .line(r#"{"type":"compaction_start","reason":"private"}"#)
                .activity,
            Some(LiveActivityKind::Compacting)
        );
        assert_eq!(
            reader
                .line(r#"{"type":"compaction_end","result":{}}"#)
                .activity,
            Some(LiveActivityKind::Working)
        );
    }

    fn usage_json(input: u64, output: u64) -> String {
        format!(
            r#"{{"input":{input},"output":{output},"cacheRead":0,"cacheWrite":0,"totalTokens":{},"cost":{{"total":0.5}}}}"#,
            input + output
        )
    }

    fn assistant(kind: &str, stop: &str, text: &str, input: u64, output: u64) -> String {
        format!(
            r#"{{"type":"{kind}","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}],"model":"m-1","usage":{},"stopReason":"{stop}"}}}}"#,
            usage_json(input, output)
        )
    }

    /// A whole settled run: header, one user message, one assistant message with two usage
    /// updates, and the end events.
    fn run(id: &str, stop: &str) -> Vec<String> {
        vec![
            header(id),
            r#"{"type":"agent_start"}"#.to_owned(),
            r#"{"type":"message_start","message":{"role":"user","content":"task"}}"#.to_owned(),
            assistant("message_start", "pending", "", 0, 0),
            format!(
                r#"{{"type":"message_update","usage":{}}}"#,
                usage_json(0, 0)
            ),
            format!(
                r#"{{"type":"message_update","usage":{}}}"#,
                usage_json(40, 2)
            ),
            assistant("message_end", stop, "done", 40, 2),
            r#"{"type":"agent_end","willRetry":false}"#.to_owned(),
            r#"{"type":"agent_settled"}"#.to_owned(),
        ]
    }

    fn read(lines: &[String]) -> (Vec<LineRead>, Box<dyn TurnReader>) {
        let mut reader: Box<dyn TurnReader> = Box::new(TurnStream::new());
        let reads = lines.iter().map(|line| reader.line(line)).collect();
        (reads, reader)
    }

    /// The turn's own usage. Pi reports no session running total, so a `Session` figure fails.
    fn turn_usage(end: &TurnEnd) -> Option<Usage> {
        end.usage.map(|scoped| match scoped {
            ScopedUsage::Turn(usage) => usage,
            ScopedUsage::Session { total, .. } => panic!("Pi reported a session total: {total:?}"),
        })
    }

    fn tokens(end: &TurnEnd) -> Option<(u64, u64)> {
        turn_usage(end).map(|usage| (usage.input.count(), usage.output.count()))
    }

    #[test]
    fn a_settled_run_completes_with_its_text_and_usage() {
        let (reads, reader) = read(&run(SESSION, "stop"));
        assert_eq!(
            reads[0].session,
            Some(SessionObservation {
                session: Some(SessionId::new(SESSION)),
                ..SessionObservation::default()
            })
        );
        assert!(reads[1..].iter().all(|read| read.session.is_none()));
        assert_eq!(
            reads[6].text,
            [TextBlock {
                session: SessionId::new(SESSION),
                text: "done".to_owned(),
            }]
        );
        let end = reader.end(EXIT_0);
        assert_eq!(end.outcome, TurnOutcome::Completed);
        assert_eq!(end.last_message.as_deref(), Some("done"));
        assert_eq!(tokens(&end), Some((40, 2)));
        assert_eq!(
            turn_usage(&end).and_then(|usage| usage.usd),
            None,
            "cost is not charged"
        );
        assert_eq!(
            end.model_usage.keys().collect::<Vec<_>>(),
            ["m-1"],
            "the model that answered carries its usage"
        );
        assert_eq!(end.session.as_ref().map(SessionId::as_str), Some(SESSION));
        assert_eq!(end.unreadable, 0);
    }

    #[test]
    fn only_the_first_header_opens_the_session() {
        let mut lines = run(SESSION, "stop");
        lines.insert(1, header("01a0e1e4-0000-7000-8000-000000000002"));
        let (reads, reader) = read(&lines);
        assert_eq!(reads[1], LineRead::default());
        assert_eq!(
            reader.end(EXIT_0).session.as_ref().map(SessionId::as_str),
            Some(SESSION)
        );
    }

    #[test]
    fn an_id_outside_pis_grammar_opens_no_session() {
        for id in ["", "-resume", "a/b", "..", "x-", "a b", "é1", "0190.jsonl"] {
            let (reads, reader) = read(&run(id, "stop"));
            assert_eq!(
                reads[0].session,
                Some(SessionObservation::default()),
                "{id:?} is observed as no session"
            );
            assert!(
                reads[6].text.is_empty(),
                "{id:?}: no session to attribute text to"
            );
            assert_eq!(reader.end(EXIT_0).session, None, "{id:?}");
        }
        for id in [SESSION, "a", "dudes_agent.1-x"] {
            let (reads, _) = read(&run(id, "stop"));
            assert_eq!(
                reads[0]
                    .session
                    .as_ref()
                    .and_then(|seen| seen.session.as_ref()),
                Some(&SessionId::new(id)),
                "control: {id:?} fits the grammar"
            );
        }
    }

    #[test]
    fn a_stream_cut_before_agent_settled_has_no_result_but_keeps_its_usage() {
        let lines = run(SESSION, "stop");
        let (_, reader) = read(&lines[..7]);
        let end = reader.end(ExitStatus {
            code: None,
            signal: Some(9),
        });
        assert_eq!(end.outcome, TurnOutcome::NoResult);
        assert_eq!(end.last_message, None);
        assert_eq!(
            tokens(&end),
            Some((40, 2)),
            "the call still consumed tokens"
        );
    }

    #[test]
    fn a_call_cut_off_mid_message_still_charges_its_last_figure() {
        let lines = run(SESSION, "stop");
        let (_, reader) = read(&lines[..6]);
        let end = reader.end(ExitStatus {
            code: None,
            signal: Some(15),
        });
        assert_eq!(end.outcome, TurnOutcome::NoResult);
        assert_eq!(tokens(&end), Some((40, 2)));
    }

    #[test]
    fn a_message_left_open_is_charged_when_the_next_one_starts() {
        let mut lines = run(SESSION, "stop");
        lines.insert(6, assistant("message_start", "pending", "", 0, 0));
        let (_, reader) = read(&lines);
        assert_eq!(tokens(&reader.end(EXIT_0)), Some((80, 4)));
    }

    fn compaction(result: &str) -> [String; 2] {
        [
            r#"{"type":"compaction_start","reason":"threshold"}"#.to_owned(),
            format!(r#"{{"type":"compaction_end","reason":"threshold"{result},"aborted":false}}"#),
        ]
    }

    #[test]
    fn a_compaction_adds_its_summary_calls_usage() {
        let mut lines = run(SESSION, "stop");
        let summary = format!(
            r#","result":{{"summary":"s","usage":{}}}"#,
            usage_json(7, 1)
        );
        lines.splice(8..8, compaction(&summary));
        let (_, reader) = read(&lines);
        let end = reader.end(EXIT_0);
        assert_eq!(tokens(&end), Some((47, 3)));
        assert_eq!(
            end.model_usage.get("m-1").map(|usage| usage.input.count()),
            Some(40),
            "the summary call names no model"
        );
    }

    #[test]
    fn a_compaction_without_a_result_leaves_the_turn_without_usage() {
        let mut lines = run(SESSION, "stop");
        lines.splice(
            8..8,
            compaction(r#","errorMessage":"Auto-compaction failed: 500""#),
        );
        let (_, reader) = read(&lines);
        let end = reader.end(EXIT_0);
        assert_eq!(end.outcome, TurnOutcome::Completed);
        assert_eq!(end.usage, None);
        assert!(
            read(&run(SESSION, "stop")).1.end(EXIT_0).usage.is_some(),
            "control: the same run without the failed compaction has usage"
        );
    }

    #[test]
    fn a_compaction_cut_off_leaves_the_turn_without_usage() {
        let mut lines = run(SESSION, "stop");
        lines.truncate(8);
        lines.push(compaction("")[0].clone());
        let (_, reader) = read(&lines);
        assert_eq!(
            reader
                .end(ExitStatus {
                    code: None,
                    signal: Some(9),
                })
                .usage,
            None
        );
    }

    #[test]
    fn a_call_cut_off_at_zero_leaves_the_turn_without_usage() {
        let mut lines = run(SESSION, "toolUse");
        lines.truncate(7);
        lines.push(assistant("message_start", "pending", "", 0, 0));
        lines.push(format!(
            r#"{{"type":"message_update","usage":{}}}"#,
            usage_json(0, 0)
        ));
        let (_, reader) = read(&lines);
        let end = reader.end(ExitStatus {
            code: None,
            signal: Some(15),
        });
        assert_eq!(end.usage, None, "the second call's figure never arrived");
        assert_eq!(
            end.model_usage.get("m-1").map(|usage| usage.input.count()),
            Some(40),
            "the finished call is still evidence"
        );
    }

    #[test]
    fn lines_after_agent_settled_belong_to_no_turn() {
        let mut lines = run(SESSION, "stop");
        lines.push(assistant("message_start", "pending", "", 0, 0));
        lines.push(assistant("message_end", "error", "late", 500, 50));
        lines.push("not json".to_owned());
        let (reads, reader) = read(&lines);
        assert!(reads[9..].iter().all(|read| *read == LineRead::default()));
        let end = reader.end(EXIT_0);
        assert_eq!(end.outcome, TurnOutcome::Completed);
        assert_eq!(tokens(&end), Some((40, 2)));
        assert_eq!(end.unreadable, 0);
    }

    #[test]
    fn a_failed_final_call_fails_the_turn_even_on_exit_0() {
        let mut lines = run(SESSION, "error");
        lines[6] = lines[6].replace(
            r#""stopReason":"error""#,
            r#""stopReason":"error","errorMessage":"401: session ended""#,
        );
        let (_, reader) = read(&lines);
        let end = reader.end(EXIT_0);
        assert_eq!(
            end.outcome,
            TurnOutcome::Failed {
                reason: "error: 401: session ended".to_owned()
            }
        );
        assert_eq!(end.last_message, None);
    }

    #[test]
    fn a_non_zero_exit_fails_a_stopped_turn() {
        let (_, reader) = read(&run(SESSION, "stop"));
        let end = reader.end(ExitStatus {
            code: Some(1),
            signal: None,
        });
        assert_eq!(
            end.outcome,
            TurnOutcome::Failed {
                reason: "exit code 1".to_owned()
            }
        );
    }

    #[test]
    fn every_stop_reason_but_stop_fails() {
        for stop in ["length", "toolUse", "aborted", "pending"] {
            let (_, reader) = read(&run(SESSION, stop));
            assert_eq!(
                reader.end(EXIT_0).outcome,
                TurnOutcome::Failed {
                    reason: stop.to_owned()
                },
                "{stop}"
            );
        }
    }

    #[test]
    fn a_turn_whose_calls_all_stayed_at_zero_has_no_usage() {
        let lines: Vec<String> = run(SESSION, "error")
            .into_iter()
            .map(|line| line.replace(&usage_json(40, 2), &usage_json(0, 0)))
            .collect();
        let (_, reader) = read(&lines);
        let end = reader.end(EXIT_0);
        assert_eq!(end.usage, None);
        assert!(end.model_usage.is_empty());
    }

    #[test]
    fn a_long_final_message_leaves_whole() {
        let long = "x".repeat(10_000);
        let mut lines = run(SESSION, "stop");
        lines[6] = assistant("message_end", "stop", &long, 40, 2);
        let (_, reader) = read(&lines);
        let message = reader.end(EXIT_0).last_message.expect("a final message");
        assert_eq!(message, long);
    }

    #[test]
    fn unreadable_lines_are_counted_not_fatal() {
        let mut lines = vec![
            String::new(),
            "not json".to_owned(),
            r#"{"no":"type"}"#.to_owned(),
        ];
        lines.extend(run(SESSION, "stop"));
        lines.insert(
            6,
            r#"{"type":"message_update","usage":{"input":"many"}}"#.to_owned(),
        );
        let (_, reader) = read(&lines);
        let end = reader.end(EXIT_0);
        assert_eq!(end.unreadable, 4);
        assert_eq!(end.outcome, TurnOutcome::Completed);
        assert_eq!(tokens(&end), Some((40, 2)));
    }
}
