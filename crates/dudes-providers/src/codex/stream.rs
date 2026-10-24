//! One managed Codex turn's standard output, read line by line.
//!
//! `thread.started` is the first line of a run and the only one that names the thread, so the
//! session id arrives at the start of the turn. No later line can disagree with it, so
//! [`TurnEnd::attribution_refused`] stays zero.
//!
//! `turn.completed.usage` is the thread's running total, not the turn's, so it leaves as
//! [`ScopedUsage::Session`]. The measurement is in `tests/fixtures/codex/README.md`.

use std::collections::{BTreeMap, BTreeSet};

use dudes_proto::providers::codex::{
    Event, ThreadStarted, TurnCompleted, TurnFailed, Usage as CodexUsage,
};
use dudes_proto::{ExitStatus, LiveActivityKind, TurnOutcome};
use dudes_types::{SessionId, Tokens, Usage};
use serde::Deserialize;

use crate::stream::{
    FinalReply, LineRead, ScopedUsage, SessionObservation, TextBlock, TextFragment, TextLayout,
    TextProgress, TurnEnd, TurnReader, exit_name,
};

/// One managed Codex turn's standard output, as far as it has been read. Pure: an equal sequence
/// of lines and an equal exit status give an equal [`TurnEnd`].
#[derive(Debug, Default)]
pub struct TurnStream {
    thread: Option<SessionObservation>,
    final_reply: Option<FinalReply>,
    ending: Option<Ending>,
    unreadable: u64,
    item_text: Option<ItemText>,
    layout: TextLayout,
    servers: BTreeSet<String>,
}

#[derive(Debug)]
struct ItemText {
    id: String,
    progress: TextProgress,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Completion {
    item: Option<CompletedItem>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CompletedItem {
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
    /// `mcp_tool_call`: the server whose tool was called.
    server: Option<String>,
}

#[derive(Debug)]
enum Ending {
    Completed(Option<CodexUsage>),
    Failed(String),
}

enum Line {
    ThreadStarted(ThreadStarted),
    ItemStarted(CompletedItem),
    ItemCompleted(CompletedItem),
    TurnCompleted(TurnCompleted),
    TurnFailed(TurnFailed),
    Other,
}

impl TurnStream {
    /// An empty reader, before the child has written anything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn session(&self) -> Option<&SessionId> {
        self.thread.as_ref()?.session.as_ref()
    }

    /// The tool server a tool-call item names, reported the first time the turn names it. Exec's
    /// stream has no event for a server that starts, so a call is the only sign of one.
    fn server_used(&mut self, item: &CompletedItem) -> Option<SessionObservation> {
        let server = item
            .server
            .as_ref()
            .filter(|server| item.kind.as_deref() == Some("mcp_tool_call") && !server.is_empty())?;
        self.servers
            .insert(server.clone())
            .then(|| SessionObservation {
                servers: vec![server.clone()],
                ..SessionObservation::default()
            })
    }

    fn completed_text(&mut self, item: CompletedItem) -> Option<TextBlock> {
        let text = item.text.filter(|text| !text.is_empty())?;
        self.final_reply = Some(FinalReply::new(&text));
        let session = self.session()?.clone();
        let fragment = if let Some(id) = item.id.filter(|id| !id.is_empty()) {
            let progress = self.item_text.get_or_insert_with(|| ItemText {
                id: id.clone(),
                progress: TextProgress::default(),
            });
            if progress.id != id {
                *progress = ItemText {
                    id,
                    progress: TextProgress::default(),
                };
            }
            progress.progress.complete(0, &text)?
        } else {
            TextFragment::BlockStart(&text)
        };
        Some(TextBlock {
            session,
            text: self.layout.render(fragment),
        })
    }
}

impl TurnReader for TurnStream {
    fn line(&mut self, text: &str) -> LineRead {
        let Some(line) = parse(text) else {
            self.unreadable += 1;
            return LineRead::default();
        };
        match line {
            Line::ThreadStarted(started) if self.thread.is_none() => {
                let observation = SessionObservation {
                    session: thread_id(&started.thread_id),
                    ..SessionObservation::default()
                };
                self.thread = Some(observation.clone());
                LineRead {
                    actions: Vec::new(),
                    omitted_text: None,
                    text: Vec::new(),
                    session: Some(observation),
                    activity: Some(LiveActivityKind::Working),
                    steps: Vec::new(),
                }
            }
            Line::ItemStarted(item) if tool_item(item.kind.as_deref().unwrap_or_default()) => {
                LineRead {
                    session: self.server_used(&item),
                    activity: Some(LiveActivityKind::UsingTool),
                    ..LineRead::default()
                }
            }
            Line::ItemCompleted(item) if tool_item(item.kind.as_deref().unwrap_or_default()) => {
                LineRead {
                    session: self.server_used(&item),
                    activity: Some(LiveActivityKind::Working),
                    ..LineRead::default()
                }
            }
            Line::ItemCompleted(item)
                if item.kind.as_deref() == Some("agent_message")
                    && item.text.as_ref().is_some_and(|text| !text.is_empty()) =>
            {
                LineRead {
                    actions: Vec::new(),
                    omitted_text: None,
                    text: self.completed_text(item).into_iter().collect(),
                    session: None,
                    activity: Some(LiveActivityKind::Working),
                    steps: Vec::new(),
                }
            }
            Line::TurnCompleted(TurnCompleted { usage }) => {
                self.ending = Some(Ending::Completed(usage));
                LineRead::default()
            }
            Line::TurnFailed(TurnFailed { error }) => {
                let reason = error
                    .map(|error| error.message)
                    .filter(|message| !message.is_empty())
                    .unwrap_or_else(|| "turn failed".to_owned());
                self.ending = Some(Ending::Failed(reason));
                LineRead::default()
            }
            Line::ThreadStarted(_)
            | Line::ItemStarted(_)
            | Line::ItemCompleted(_)
            | Line::Other => LineRead::default(),
        }
    }

    fn end(self: Box<Self>, exit: ExitStatus) -> TurnEnd {
        let Self {
            thread,
            final_reply,
            ending,
            unreadable,
            item_text: _,
            layout: _,
            servers: _,
        } = *self;
        // The vendor writes all zeros when no token counts reached it (`usage_from_last_total` in
        // its exec event processor). That is no information, and an end with no usage keeps it off
        // `Some(Usage)`.
        let usage = match &ending {
            Some(Ending::Completed(Some(usage))) if *usage != CodexUsage::default() => {
                Some(ScopedUsage::Session {
                    total: to_usage(usage),
                    calls: Usage::ZERO,
                })
            }
            _ => None,
        };
        let outcome = match ending {
            None => TurnOutcome::NoResult,
            Some(Ending::Failed(reason)) => TurnOutcome::Failed { reason },
            Some(Ending::Completed(_)) if exit.is_success() => TurnOutcome::Completed,
            Some(Ending::Completed(_)) => TurnOutcome::Failed {
                reason: exit_name(exit),
            },
        };
        let final_reply = final_reply.filter(|_| outcome == TurnOutcome::Completed);
        let last_message = final_reply.as_ref().map(|reply| reply.text.clone());
        TurnEnd {
            final_observation: None,
            outcome,
            usage,
            model_usage: BTreeMap::new(),
            last_message,
            final_reply,
            usage_refused: None,
            session: thread.and_then(|observation| observation.session),
            attribution_refused: 0,
            unreadable,
        }
    }
}

/// `None` for a line that is not a JSON object with a string `type`, or whose known payload does
/// not match its shape. Each read goes into a typed struct, never a `Value`, so the fields no
/// payload types, such as a command's output, are skipped without being built.
fn parse(text: &str) -> Option<Line> {
    let Event { kind } = serde_json::from_str(text).ok()?;
    Some(match kind?.as_str() {
        "thread.started" => Line::ThreadStarted(serde_json::from_str(text).ok()?),
        "item.started" => Line::ItemStarted(
            serde_json::from_str::<Completion>(text)
                .ok()?
                .item
                .unwrap_or_default(),
        ),
        "item.completed" => Line::ItemCompleted(
            serde_json::from_str::<Completion>(text)
                .ok()?
                .item
                .unwrap_or_default(),
        ),
        "turn.completed" => Line::TurnCompleted(serde_json::from_str(text).ok()?),
        "turn.failed" => Line::TurnFailed(serde_json::from_str(text).ok()?),
        _ => Line::Other,
    })
}

fn tool_item(kind: &str) -> bool {
    matches!(
        kind,
        "command_execution" | "file_change" | "mcp_tool_call" | "web_search"
    )
}

/// The id as a session when it has the form of each thread id in the captured runs: the
/// hyphenated text of a UUID in lowercase, five groups of 8, 4, 4, 4 and 12 hexadecimal digits.
///
/// Anything else is dropped before it becomes a [`SessionId`]. `codex exec resume`
/// reads an argument that starts with `-` as an option and one that is not a UUID as a thread
/// name, so resuming such an id would reach another thread or none.
pub(super) fn thread_id(text: &str) -> Option<SessionId> {
    let mut groups = text.split('-');
    let well_formed = [8, 4, 4, 4, 12].into_iter().all(|len| {
        groups.next().is_some_and(|group| {
            group.len() == len
                && group
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
    }) && groups.next().is_none();
    well_formed.then(|| SessionId::new(text))
}

/// Dudes counts the cache apart from the input, and Codex counts it inside, so it comes out here.
fn to_usage(usage: &CodexUsage) -> Usage {
    Usage {
        input: Tokens::new(
            usage
                .input_tokens
                .saturating_sub(usage.cached_input_tokens)
                .saturating_sub(usage.cache_write_input_tokens),
        ),
        output: Tokens::new(usage.output_tokens),
        cache_read: Tokens::new(usage.cached_input_tokens),
        cache_creation: Tokens::new(usage.cache_write_input_tokens),
        usd: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::MAX_TEXT_BYTES;

    const EXIT_0: ExitStatus = ExitStatus {
        code: Some(0),
        signal: None,
    };

    /// The thread id of the captured success run, `tests/fixtures/codex/c1-success.stdout`.
    const THREAD: &str = "01a0e1d9-3860-7800-a9ff-2994ef3b1c6f";

    fn started(id: &str) -> String {
        format!(r#"{{"type":"thread.started","thread_id":"{id}"}}"#)
    }

    fn message(text: &str) -> String {
        format!(
            r#"{{"type":"item.completed","item":{{"id":"item_0","type":"agent_message","text":"{text}"}}}}"#
        )
    }

    fn completed(usage: [u64; 4]) -> String {
        let [input, cached, written, output] = usage;
        format!(
            r#"{{"type":"turn.completed","usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":{written},"output_tokens":{output},"reasoning_output_tokens":0}}}}"#
        )
    }

    /// Exec's stream has no event for a server that starts, so the first tool call naming a server
    /// is what reaches the supervisor, once per server per turn. Mutants: report nothing, or
    /// report each call, which would raise one rung per call.
    #[test]
    fn the_first_call_to_each_mcp_server_is_reported() {
        let call = |phase: &str, server: &str| {
            format!(
                r#"{{"type":"item.{phase}","item":{{"id":"item_0","type":"mcp_tool_call","server":"{server}","tool":"echo","status":"in_progress"}}}}"#
            )
        };
        let mut reader = TurnStream::new();
        reader.line(&started(THREAD));
        let servers = |read: LineRead| read.session.map(|observed| observed.servers);
        assert_eq!(
            servers(reader.line(&call("started", "memory"))),
            Some(vec!["memory".to_owned()])
        );
        assert_eq!(servers(reader.line(&call("completed", "memory"))), None);
        assert_eq!(
            servers(reader.line(&call("completed", "rogue"))),
            Some(vec!["rogue".to_owned()])
        );
        let other = r#"{"type":"item.started","item":{"type":"command_execution","server":"x"}}"#;
        assert_eq!(servers(reader.line(other)), None);
    }

    #[test]
    fn structured_tool_events_emit_only_fixed_activity_kinds() {
        let mut reader = TurnStream::new();
        assert_eq!(
            reader.line(&started(THREAD)).activity,
            Some(LiveActivityKind::Working)
        );
        let started = r#"{"type":"item.started","item":{"type":"command_execution","command":"private path"}}"#;
        assert_eq!(
            reader.line(started).activity,
            Some(LiveActivityKind::UsingTool)
        );
        let finished = r#"{"type":"item.completed","item":{"type":"command_execution","aggregated_output":"private text"}}"#;
        assert_eq!(
            reader.line(finished).activity,
            Some(LiveActivityKind::Working)
        );
        assert_eq!(
            reader
                .line(r#"{"type":"item.started","item":{"type":"unknown","text":"private"}}"#)
                .activity,
            None
        );
    }

    fn read(lines: &[String], exit: ExitStatus) -> (Vec<LineRead>, TurnEnd) {
        let mut reader: Box<dyn TurnReader> = Box::new(TurnStream::new());
        let reads = lines.iter().map(|line| reader.line(line)).collect();
        (reads, reader.end(exit))
    }

    fn opened(id: &str) -> SessionObservation {
        SessionObservation {
            session: Some(SessionId::new(id)),
            ..SessionObservation::default()
        }
    }

    /// a learned id that is empty, that the vendor would read as an option, or that
    /// breaks Codex's id grammar opens the session with no id, attributes no text, and ends the
    /// turn with no session.
    ///
    /// The grammar comes from the thread ids in `tests/fixtures/codex/`: each is 36 bytes, five
    /// groups of lowercase hexadecimal digits of lengths 8, 4, 4, 4 and 12 joined by `-`, which is
    /// the hyphenated text of a UUID.
    #[test]
    fn a_thread_id_outside_codex_grammar_opens_no_session() {
        let refused = [
            ("empty", ""),
            ("an option", "--last"),
            (
                "an option in the id's shape",
                "-1a0e1d9-3860-7800-a9ff-2994ef3b1c6f",
            ),
            ("uppercase", "01A0E1D9-3860-7800-A9FF-2994EF3B1C6F"),
            ("not hexadecimal", "01a0e1d9-3860-7800-a9ff-2994ef3b1c6g"),
            ("a group short", "01a0e1d9-3860-7800-a9ff-2994ef3b1c6"),
            ("a group missing", "01a0e1d9-3860-7800-2994ef3b1c6f"),
            ("a group extra", "01a0e1d9-3860-7800-a9ff-2994ef3b1c6f-00"),
            ("no hyphens", "01a0e1d938607800a9ff2994ef3b1c6f"),
            ("a thread name", "my-thread"),
            ("padded", " 01a0e1d9-3860-7800-a9ff-2994ef3b1c6f"),
        ];
        for (shape, id) in refused {
            let (reads, end) = read(&[started(id), message("ok")], EXIT_0);
            assert_eq!(
                reads[0].session,
                Some(SessionObservation::default()),
                "{shape}: the session opens with no id"
            );
            assert_eq!(reads[1].text, [], "{shape}: no text is attributed");
            assert_eq!(end.session, None, "{shape}: the turn ends with no session");
        }

        let (reads, end) = read(&[started(THREAD), message("ok")], EXIT_0);
        assert_eq!(
            reads[0].session,
            Some(opened(THREAD)),
            "control: the captured id opens the session"
        );
        assert_eq!(
            reads[1].text,
            [TextBlock {
                session: SessionId::new(THREAD),
                text: "ok".to_owned(),
            }]
        );
        assert_eq!(end.session, Some(SessionId::new(THREAD)));
    }

    #[test]
    fn only_the_first_thread_started_opens_the_session() {
        let other = "01a0e1d9-84d5-7bc2-8567-d32735103d71";
        let (reads, end) = read(&[started(THREAD), started(other)], EXIT_0);
        assert_eq!(reads[0].session, Some(opened(THREAD)));
        assert_eq!(reads[1].session, None);
        assert_eq!(end.session, Some(SessionId::new(THREAD)));

        let (reads, end) = read(&[started(""), started(THREAD)], EXIT_0);
        assert_eq!(reads[0].session, Some(SessionObservation::default()));
        assert_eq!(
            reads[1].session, None,
            "a later valid id does not reopen it"
        );
        assert_eq!(end.session, None);
    }

    /// The figures are the vendor's own test of its usage parsing: 100 input tokens, of which 40
    /// were read from the cache and 60 written to it.
    #[test]
    fn cached_and_written_tokens_come_out_of_the_input() {
        let (_, end) = read(&[started(THREAD), completed([100, 40, 60, 10])], EXIT_0);
        assert_eq!(
            end.usage,
            Some(ScopedUsage::Session {
                total: Usage {
                    input: Tokens::new(0),
                    output: Tokens::new(10),
                    cache_read: Tokens::new(40),
                    cache_creation: Tokens::new(60),
                    usd: None,
                },
                calls: Usage::ZERO,
            })
        );
    }

    #[test]
    fn an_all_zero_usage_accounts_nothing() {
        let (_, end) = read(&[started(THREAD), completed([0; 4])], EXIT_0);
        assert_eq!(end.usage, None);
        assert_eq!(end.outcome, TurnOutcome::Completed);

        let (_, end) = read(&[started(THREAD), completed([0, 0, 0, 1])], EXIT_0);
        assert_eq!(
            end.usage,
            Some(ScopedUsage::Session {
                total: Usage {
                    output: Tokens::new(1),
                    ..Usage::ZERO
                },
                calls: Usage::ZERO,
            }),
            "control: one counted token is reported"
        );
    }

    #[test]
    fn the_last_agent_message_is_the_final_message() {
        let lines = [
            started(THREAD),
            message("first"),
            r#"{"type":"item.completed","item":{"id":"item_1","type":"reasoning","text":"hidden"}}"#
                .to_owned(),
            r#"{"type":"item.started","item":{"id":"item_2","type":"agent_message","text":"partial"}}"#
                .to_owned(),
            r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"second"}}"#
                .to_owned(),
            message(""),
            completed([10, 0, 0, 1]),
        ];
        let (reads, end) = read(&lines, EXIT_0);
        let text: Vec<&str> = reads
            .iter()
            .flat_map(|read| &read.text)
            .map(|block| block.text.as_str())
            .collect();
        assert_eq!(text, ["first", "\nsecond"]);
        assert_eq!(end.outcome, TurnOutcome::Completed);
        assert_eq!(end.last_message.as_deref(), Some("second"));

        let long = "x".repeat(2 * MAX_TEXT_BYTES);
        let (_, end) = read(&[message(&long), completed([10, 0, 0, 1])], EXIT_0);
        assert_eq!(end.last_message, Some(long));
    }

    #[test]
    fn a_failed_turn_ends_failed_with_the_vendor_message() {
        let failed = |error: &str| {
            let lines = [
                started(THREAD),
                message("partial"),
                format!(r#"{{"type":"turn.failed"{error}}}"#),
            ];
            read(
                &lines,
                ExitStatus {
                    code: Some(1),
                    signal: None,
                },
            )
            .1
        };
        let end = failed(r#","error":{"message":"model not found"}"#);
        assert_eq!(
            end.outcome,
            TurnOutcome::Failed {
                reason: "model not found".to_owned()
            }
        );
        assert_eq!(end.usage, None);
        assert_eq!(end.last_message, None);

        for missing in ["", r#","error":null"#, r#","error":{"message":""}"#] {
            assert_eq!(
                failed(missing).outcome,
                TurnOutcome::Failed {
                    reason: "turn failed".to_owned()
                },
                "{missing}"
            );
        }

        let long = "x".repeat(2 * MAX_TEXT_BYTES);
        let TurnOutcome::Failed { reason } =
            failed(&format!(r#","error":{{"message":"{long}"}}"#)).outcome
        else {
            panic!("a turn.failed line ends the turn Failed");
        };
        assert_eq!(reason, long);
    }

    /// The tokens were spent whatever the exit status, so they are still reported.
    #[test]
    fn a_completed_turn_with_a_failing_exit_is_failed() {
        let lines = [started(THREAD), message("ok"), completed([10, 0, 0, 1])];
        for (exit, reason) in [
            (
                ExitStatus {
                    code: Some(1),
                    signal: None,
                },
                "exit code 1",
            ),
            (
                ExitStatus {
                    code: None,
                    signal: Some(15),
                },
                "signal 15",
            ),
        ] {
            let (_, end) = read(&lines, exit);
            assert_eq!(
                end.outcome,
                TurnOutcome::Failed {
                    reason: reason.to_owned()
                }
            );
            assert_eq!(end.last_message, None);
            assert!(
                matches!(end.usage, Some(ScopedUsage::Session { .. })),
                "{reason}"
            );
        }
    }

    #[test]
    fn a_stream_with_no_ending_has_no_result() {
        let (_, end) = read(&[started(THREAD), message("ok")], EXIT_0);
        assert_eq!(end.outcome, TurnOutcome::NoResult);
        assert_eq!(end.usage, None);
        assert_eq!(end.last_message, None);
        assert_eq!(end.session, Some(SessionId::new(THREAD)));
    }

    /// A negative count is a drifted payload, so that `turn.completed` is counted as unreadable
    /// and does not end the turn.
    #[test]
    fn unreadable_lines_are_counted_not_fatal() {
        let lines = [
            "not json".to_owned(),
            String::new(),
            r#"{"thread_id":"01a0e1d9-3860-7800-a9ff-2994ef3b1c6f"}"#.to_owned(),
            r#"{"type":"turn.completed","usage":{"input_tokens":-1}}"#.to_owned(),
            started(THREAD),
            r#"{"type":"turn.started"}"#.to_owned(),
            r#"{"type":"a.future.event","field":1}"#.to_owned(),
            completed([10, 0, 0, 1]),
        ];
        let (_, end) = read(&lines, EXIT_0);
        assert_eq!(end.unreadable, 4);
        assert_eq!(end.outcome, TurnOutcome::Completed);
        assert!(end.usage.is_some(), "the turn still ends with usage");
    }
}
