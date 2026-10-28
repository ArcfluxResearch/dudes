//! Claude Code in managed mode: one `--output-format stream-json` line at a time.
//!
//! Every line is dispatched on its envelope (`type`, then `subtype` for `system`) into the
//! matching `dudes_proto::providers::claude` payload. Unknown types and subtypes are kept, not
//! rejected: a vendor release that adds a line kind must not break a running agent.
//!
//! Limit stated for the record: this crate proves what the adapter concludes from the lines
//! and the exit status. That the supervisor asks it on `ChildExited` rather than waiting for a
//! hook event is the supervisor's behaviour to prove.

pub mod config;
mod hooks;
pub mod managed;
pub mod outcome;
mod steps;
pub mod stream;
mod text;
pub mod usage;

pub use hooks::{HookTranslationError, encode_reply, requires_reply, translate};
pub use managed::{ClaudeAdapter, SESSION_ACQUISITION};

use dudes_proto::providers::claude::{
    HookLifecycle, MessageLine, RateLimitEvent, ResultLine, StreamEnvelope, SystemInit,
    ThinkingTokens,
};
use serde::Deserialize;
use serde_json::Value;

/// A line that could not be parsed. Shape errors name the line kind so the journal can say
/// which payload drifted.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// Not JSON at all.
    #[error("stream line is not JSON: {0}")]
    Json(#[source] serde_json::Error),
    /// JSON, but not an object with a string `type`.
    #[error("stream line is not an object with a `type`")]
    NoKind,
    /// The payload of a known kind did not deserialize.
    #[error("`{kind}` line does not match its payload shape: {source}")]
    Shape {
        /// `type`, or `type/subtype` for `system`.
        kind: String,
        /// What serde rejected.
        #[source]
        source: serde_json::Error,
    },
}

/// The typed body of one line.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamLine {
    /// `system/init`: where the session id is learned.
    Init(SystemInit),
    /// `system/hook_started`, `hook_progress`, `hook_response`.
    Hook(HookLifecycle),
    /// `system/thinking_tokens`.
    ThinkingTokens(ThinkingTokens),
    /// Any other `system` line (`notification`, `api_retry`, or one not seen yet), verbatim.
    System {
        /// The subtype, if any.
        subtype: Option<String>,
        /// The whole line.
        raw: Value,
    },
    /// `assistant`.
    Assistant(MessageLine),
    /// `user`.
    User(MessageLine),
    /// `rate_limit_event`.
    RateLimit(RateLimitEvent),
    /// `result`: the last line of a turn.
    Result(ResultLine),
    /// A `type` this adapter does not know, verbatim.
    Unknown {
        /// The whole line.
        raw: Value,
    },
}

/// One parsed line: the common envelope plus the typed body.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    /// `type`, `subtype`, `session_id`, `uuid`.
    pub envelope: StreamEnvelope,
    /// The payload.
    pub body: StreamLine,
}

impl Line {
    /// The session this line belongs to, from the envelope.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.envelope.session_id.as_deref()
    }
}

/// Only top-level, non-synthetic assistant text from a typed stream line.
/// The session travels with each block so a caller cannot detach text from attribution.
pub fn assistant_text(line: &Line) -> impl Iterator<Item = (&str, &str)> {
    let session = line.session_id().filter(|session| !session.is_empty());
    let message = match &line.body {
        StreamLine::Assistant(message)
            if message.message.role == "assistant"
                && message.parent_tool_use_id.is_none()
                && message.is_synthetic != Some(true) =>
        {
            Some(message)
        }
        _ => None,
    };
    message
        .into_iter()
        .flat_map(|message| &message.message.content)
        .filter_map(move |block| {
            if block.kind != "text" {
                return None;
            }
            session.zip(block.text.as_deref().filter(|text| !text.is_empty()))
        })
}

/// Parse one line of the stream.
///
/// A blank or whitespace-only line is not JSON and returns [`ParseError::Json`]; the host
/// reading the pipe decides whether to skip it. A line whose envelope fields have the wrong
/// type (a non-string `subtype`, say) is [`ParseError::Shape`] with kind `envelope`.
pub fn parse_line(text: &str) -> Result<Line, ParseError> {
    let value: Value = serde_json::from_str(text).map_err(ParseError::Json)?;
    if value.get("type").and_then(Value::as_str).is_none() {
        return Err(ParseError::NoKind);
    }
    let envelope: StreamEnvelope = payload(&value, "envelope")?;
    let subtype = envelope.subtype.as_deref();
    let body = match (envelope.kind.as_str(), subtype) {
        ("system", Some("init")) => StreamLine::Init(payload(&value, "system/init")?),
        ("system", Some(sub @ ("hook_started" | "hook_progress" | "hook_response"))) => {
            StreamLine::Hook(payload(&value, &format!("system/{sub}"))?)
        }
        ("system", Some("thinking_tokens")) => {
            StreamLine::ThinkingTokens(payload(&value, "system/thinking_tokens")?)
        }
        ("system", _) => StreamLine::System {
            subtype: envelope.subtype.clone(),
            raw: value,
        },
        ("assistant", _) => StreamLine::Assistant(payload(&value, "assistant")?),
        ("user", _) => StreamLine::User(payload(&value, "user")?),
        ("rate_limit_event", _) => StreamLine::RateLimit(payload(&value, "rate_limit_event")?),
        ("result", _) => StreamLine::Result(payload(&value, "result")?),
        _ => StreamLine::Unknown { raw: value },
    };
    Ok(Line { envelope, body })
}

fn payload<'de, T: Deserialize<'de>>(value: &'de Value, kind: &str) -> Result<T, ParseError> {
    T::deserialize(value).map_err(|source| ParseError::Shape {
        kind: kind.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use dudes_types::{AgentId, ModelSelection, SessionId};

    use super::*;
    use crate::adapter::{Adapter, AgentCtx, TurnSession};
    use crate::launch::EnvVar;

    const AGENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const UUID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const AGENT_DIR: &str = "//fixture.invalid/dudes data/agents/01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const HOOK_COMMAND: &str = "//fixture.invalid/dudes/bin/dudes-hook";
    const BODIES: [&str; 5] = [
        "task-body-4d0c0e58",
        "prompt-body-3816703a",
        "mail-body-399e17c4",
        "answer-body-c33c7163",
        "steer-body-5d047b24",
    ];

    /// Invariant 6: an attached launch resumes a confirmed session through the
    /// provider's hook settings, while every message body remains outside argv and stdin.
    #[test]
    fn attached_launch_composer_never_carries_message_bodies() {
        let adapter = ClaudeAdapter::new(config::ClaudeConfig {
            model: Some(BODIES[1].to_owned()),
            disallowed_tools: vec![BODIES[3].to_owned()],
            allowed_tools: vec![BODIES[2].to_owned()],
            tools: vec![BODIES[4].to_owned()],
            extra_args: BODIES.iter().map(OsString::from).collect(),
            ..config::ClaudeConfig::default()
        });
        let daemon_env = [
            EnvVar::new("SAFE_NAME", "ambient-value"),
            EnvVar::new("CLAUDECODE", "nested-session"),
        ];
        let ctx = AgentCtx {
            agent: AgentId::parse(AGENT).expect("fixture agent id"),
            cwd: Path::new("//fixture.invalid/work tree"),
            agent_dir: Path::new(AGENT_DIR),
            daemon_env: &daemon_env,
            sandbox: None,
            trusted: true,
            max_turns: Some(7),
            max_budget: None,
            mcp_servers: &[],
        };
        let adapter: &dyn Adapter = &adapter;

        let spec = adapter
            .attached(
                &ctx,
                &TurnSession::Resume(SessionId::from(UUID)),
                &ModelSelection::provider_default(),
            )
            .expect("Claude is attachable with a vendor-confirmed session");
        let hook_settings = Path::new(AGENT_DIR)
            .join("hooks/settings.json")
            .into_os_string();
        assert_eq!(
            spec.argv,
            vec![
                OsString::from("claude"),
                OsString::from("--resume"),
                OsString::from(UUID),
                OsString::from("--settings"),
                hook_settings.clone(),
            ],
            "the positive golden is the complete attached argv"
        );
        assert_eq!(
            spec.stdin,
            crate::launch::LaunchInput::Null,
            "an attached launch has no input body"
        );
        assert_eq!(spec.cwd, ctx.cwd);
        assert_eq!(
            spec.env,
            vec![EnvVar::new("SAFE_NAME", "ambient-value")],
            "attached and managed launches share the nesting scrub"
        );

        let install = adapter.install_hooks(&ctx, Path::new(HOOK_COMMAND));
        assert_eq!(install.files.len(), 1, "Claude owns one hook settings file");
        assert_eq!(
            install.files[0].0,
            Path::new(AGENT_DIR).join("hooks/settings.json")
        );
        assert_eq!(
            spec.argv[4], hook_settings,
            "the launch names the file the provider installs"
        );

        let sentinel = adapter
            .wake_sentinel()
            .expect("Claude can wake an idle attached session without a message body");
        assert_eq!(sentinel, "[dudes] you have unread mail in inbox/");
        for body in BODIES {
            assert!(
                spec.argv
                    .iter()
                    .all(|part| !part.to_string_lossy().contains(body)),
                "attached argv carried planted body `{body}`"
            );
            assert!(
                match &spec.stdin {
                    crate::launch::LaunchInput::Null => true,
                    crate::launch::LaunchInput::WriteThenClose(stdin) => !stdin
                        .windows(body.len())
                        .any(|window| window == body.as_bytes()),
                    crate::launch::LaunchInput::Protocol(_) => false,
                },
                "attached stdin carried planted body `{body}`"
            );
            assert!(
                !sentinel.contains(body),
                "the content-free sentinel carried planted body `{body}`"
            );
        }

        let minted = TurnSession::First(SessionId::from(UUID));
        assert!(
            adapter
                .attached(&ctx, &minted, &ModelSelection::provider_default())
                .is_none(),
            "a daemon-minted session is not vendor-confirmed and cannot be attached"
        );
    }

    /// An attached launch passes the agent's model as two typed parts at the end of the argv, and
    /// nothing when the agent is on the engine default.
    #[test]
    fn attached_launch_ends_in_the_agents_model() {
        let adapter = ClaudeAdapter::new(config::ClaudeConfig::default());
        let ctx = AgentCtx {
            agent: AgentId::parse(AGENT).expect("fixture agent id"),
            cwd: Path::new("//fixture.invalid/work tree"),
            agent_dir: Path::new(AGENT_DIR),
            daemon_env: &[],
            sandbox: None,
            trusted: true,
            max_turns: None,
            max_budget: None,
            mcp_servers: &[],
        };
        let adapter: &dyn Adapter = &adapter;
        let session = TurnSession::Resume(SessionId::from(UUID));
        let spec = adapter
            .attached(&ctx, &session, &ModelSelection::provider_default())
            .expect("attachable");
        let on_sonnet = adapter
            .attached(&ctx, &session, &ModelSelection::named("sonnet"))
            .expect("attachable on a named model");
        assert_eq!(
            on_sonnet.argv[..5],
            spec.argv[..],
            "a named model leaves the rest of the argv alone"
        );
        assert_eq!(
            on_sonnet.argv[5..],
            [OsString::from("--model"), OsString::from("sonnet")],
            "a named model ends the argv with two typed parts"
        );
        assert!(
            adapter
                .attached(
                    &ctx,
                    &session,
                    &ModelSelection::named("--dangerously-skip-permissions"),
                )
                .is_none(),
            "a name that reads as a flag is never passed"
        );
    }

    /// Forward compatibility: a `type` and a `system` subtype this adapter has
    /// never seen both parse, keeping the line verbatim, instead of erroring.
    #[test]
    fn forward_compat() {
        let line = parse_line(r#"{"type":"something_new","session_id":"s1","x":1}"#).unwrap();
        assert_eq!(line.session_id(), Some("s1"));
        assert!(matches!(line.body, StreamLine::Unknown { ref raw } if raw["x"] == 1));

        let line =
            parse_line(r#"{"type":"system","subtype":"api_retry","attempt":2,"session_id":"s1"}"#)
                .unwrap();
        assert!(matches!(
            line.body,
            StreamLine::System { ref subtype, ref raw }
                if subtype.as_deref() == Some("api_retry") && raw["attempt"] == 2
        ));
    }

    /// A line that is not JSON (blank lines included), or JSON without a `type`, is a named
    /// error rather than a silently dropped line.
    #[test]
    fn malformed_lines_are_named_errors() {
        assert!(matches!(parse_line("not json"), Err(ParseError::Json(_))));
        assert!(matches!(parse_line(""), Err(ParseError::Json(_))));
        assert!(matches!(parse_line("  \t"), Err(ParseError::Json(_))));
        assert!(matches!(
            parse_line(r#"{"subtype":"init"}"#),
            Err(ParseError::NoKind)
        ));
        assert!(matches!(parse_line("[1,2]"), Err(ParseError::NoKind)));
    }

    /// A known kind whose payload drifted is `Shape`, naming the kind so the journal can say
    /// which payload it was — the envelope included, so a wrong-typed `subtype` is not
    /// mistaken for a non-JSON line.
    #[test]
    fn payload_drift_is_a_shape_error_naming_the_kind() {
        assert!(matches!(
            parse_line(r#"{"type":"result","is_error":"yes"}"#),
            Err(ParseError::Shape { ref kind, .. }) if kind == "result"
        ));
        assert!(matches!(
            parse_line(r#"{"type":"system","subtype":7}"#),
            Err(ParseError::Shape { ref kind, .. }) if kind == "envelope"
        ));
        assert!(matches!(
            parse_line(r#"{"type":"system","subtype":"init","tools":"none"}"#),
            Err(ParseError::Shape { ref kind, .. }) if kind == "system/init"
        ));
    }

    /// The session id is read from the envelope, so lines whose payload struct has no
    /// `session_id` field (hook lifecycle, messages) still report it.
    #[test]
    fn session_id_comes_from_the_envelope() {
        let line = parse_line(
            r#"{"type":"system","subtype":"hook_started","hook_id":"h","hook_name":"SessionStart:startup","hook_event":"SessionStart","session_id":"s1"}"#,
        )
        .unwrap();
        assert!(matches!(line.body, StreamLine::Hook(ref h) if h.hook_event == "SessionStart"));
        assert_eq!(line.session_id(), Some("s1"));
    }

    #[test]
    fn assistant_text_admits_only_top_level_nonsynthetic_typed_text_with_session() {
        let valid = r#"{"type":"assistant","session_id":"s1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret"},{"type":"text","text":"visible"},{"type":"tool_use","name":"Read","input":{"secret":"private"}},{"type":"text","text":""},{"type":"text","text":"again"}]}}"#;
        assert_eq!(
            assistant_text(&parse_line(valid).unwrap()).collect::<Vec<_>>(),
            [("s1", "visible"), ("s1", "again")]
        );
        for raw in [
            r#"{"type":"assistant","session_id":"s1","isSynthetic":true,"message":{"role":"assistant","content":[{"type":"text","text":"hidden"}]}}"#,
            r#"{"type":"assistant","session_id":"s1","parent_tool_use_id":"tool","message":{"role":"assistant","content":[{"type":"text","text":"hidden"}]}}"#,
            r#"{"type":"assistant","session_id":"s1","message":{"role":"user","content":[{"type":"text","text":"hidden"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hidden"}]}}"#,
            r#"{"type":"user","session_id":"s1","message":{"role":"assistant","content":[{"type":"text","text":"hidden"}]}}"#,
            r#"{"type":"result","session_id":"s1","result":"hidden"}"#,
            r#"{"type":"unknown","session_id":"s1","text":"hidden"}"#,
        ] {
            assert!(
                assistant_text(&parse_line(raw).unwrap()).next().is_none(),
                "refused line: {raw}"
            );
        }
    }
}
