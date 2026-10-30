//! Claude hook settings as a pure host-written projection.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;

use dudes_proto::providers::claude::{
    HookHandler, HookInput, HookMatcher, HookOutput, HookSpecificOutput, Settings,
};
use dudes_proto::reply::{HookDecision, HookReply, VendorReply};
use dudes_proto::{Event, EventKind};
use dudes_types::{AgentId, ModelSelection, Provenance, Timestamp};
use serde_json::Value;

use crate::adapter::{AgentCtx, HookInstall, TurnSession};
use crate::claude::managed::{SESSION_ACQUISITION, scrub_env};
use crate::launch::LaunchSpec;

const HOOK_SETTINGS_FILE: &str = "hooks/settings.json";

/// The only provider-originated text that may reach an attached Claude PTY.
pub(super) const WAKE_SENTINEL: &str = "[dudes] you have unread mail in inbox/";

/// The provider-owned settings path shared by hook installation and attached launch composition.
pub(super) fn settings_path(ctx: &AgentCtx<'_>) -> std::path::PathBuf {
    ctx.agent_dir.join(HOOK_SETTINGS_FILE)
}

/// Compose a TUI resume from typed parts. A daemon-minted session is not vendor-confirmed, so it
/// produces no launch. Message bodies, managed settings and managed flags are not inputs.
pub(super) fn compose_attached(
    command: &OsStr,
    ctx: &AgentCtx<'_>,
    session: &TurnSession,
    model: &ModelSelection,
) -> Option<LaunchSpec> {
    let TurnSession::Resume(_) = session else {
        return None;
    };
    let mut argv = vec![command.to_owned()];
    argv.extend(SESSION_ACQUISITION.argv(session).ok()?);
    argv.extend([
        OsString::from("--settings"),
        settings_path(ctx).into_os_string(),
    ]);
    for dir in ctx.extra_dirs {
        argv.extend([OsString::from("--add-dir"), dir.clone().into_os_string()]);
    }
    if let Some(name) = model.selector() {
        // A leading dash would read as a flag.
        if name.is_empty() || name.starts_with('-') {
            return None;
        }
        argv.extend([OsString::from("--model"), OsString::from(name)]);
    }
    Some(LaunchSpec {
        argv,
        env: scrub_env(ctx.daemon_env),
        cwd: ctx.cwd.to_path_buf(),
        stdin: crate::launch::LaunchInput::Null,
    })
}

const HOOK_EVENTS: [&str; 10] = [
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "SubagentStop",
    "Notification",
    "PreCompact",
    "PostCompact",
];

/// Why a Claude hook payload could not become a normalized event.
#[derive(Debug, thiserror::Error)]
pub enum HookTranslationError {
    /// The vendor payload had the wrong JSON shape for [`HookInput`].
    #[error("Claude hook payload does not match its input shape: {0}")]
    Shape(#[source] serde_json::Error),
    /// The payload named an event outside the ten this adapter models.
    #[error("Claude hook event `{0}` is not modeled")]
    Unrecognised(String),
}

/// Translate one Claude hook payload without trusting any identity field it contains.
pub fn translate(
    agent: AgentId,
    at: Timestamp,
    raw: &Value,
) -> Result<Event, HookTranslationError> {
    let input: HookInput =
        serde_json::from_value(raw.clone()).map_err(HookTranslationError::Shape)?;
    let kind = match input.hook_event_name.as_str() {
        "SessionStart" => EventKind::SessionStart,
        "SessionEnd" => EventKind::SessionEnd,
        "UserPromptSubmit" => EventKind::PromptSubmit,
        "PreToolUse" => EventKind::PreTool {
            name: input.tool_name.unwrap_or_default(),
            input: input.tool_input.unwrap_or(Value::Null),
        },
        // This modeled event fires only after a successful tool call. Failures have a distinct
        // vendor event that Dudes does not install.
        "PostToolUse" => EventKind::PostTool {
            name: input.tool_name.unwrap_or_default(),
            ok: true,
        },
        "Stop" => EventKind::TurnEnd {
            last_message: input.last_assistant_message,
        },
        "SubagentStop" => EventKind::SubagentEnd,
        "Notification" => EventKind::Notification {
            text: input.message.unwrap_or_default(),
        },
        "PreCompact" => EventKind::CompactStart,
        "PostCompact" => EventKind::CompactEnd,
        other => return Err(HookTranslationError::Unrecognised(other.to_owned())),
    };
    Ok(Event {
        agent,
        at,
        provenance: Provenance::Observed,
        kind,
        raw: Some(raw.clone()),
    })
}

/// Whether Claude can act on this event's reply.
#[must_use]
pub const fn requires_reply(event: &Event) -> bool {
    !matches!(
        event.kind,
        EventKind::SessionEnd | EventKind::Notification { .. } | EventKind::CompactEnd
    )
}

/// Encode the supervisor's provider-neutral reply for Claude's hook contract.
///
/// At a stop boundary — `Stop` and `SubagentStop` — the vendor offers exactly three levers that
/// keep the turn going: a top-level `decision`, exit code `2`, and `hookSpecificOutput`'s
/// `additionalContext`. `Deny` means the loop is paused, so it touches none of the three and
/// emits no body at all, whatever `reply.context` holds; omitting `decision` is the documented
/// way to let the model stop. `Block` is the opposite intent and keeps `decision: "block"` with
/// exit `2`. The two therefore never encode equal. The stop-boundary rows are the two stop cells;
/// a fourth keep-working lever appearing at that boundary voids them.
#[must_use]
pub fn encode_reply(reply: &HookReply, event: &Event) -> VendorReply {
    if !requires_reply(event) {
        return VendorReply {
            body: None,
            exit_code: 0,
        };
    }

    let mut output = HookOutput::default();
    let mut specific = reply.context.as_ref().map(|context| HookSpecificOutput {
        hook_event_name: event_name(&event.kind).to_owned(),
        additional_context: Some(context.clone()),
        ..HookSpecificOutput::default()
    });
    let exit_code = match &reply.decision {
        HookDecision::Allow => 0,
        HookDecision::Deny { reason } => match &event.kind {
            EventKind::PreTool { .. } => {
                let detail = specific.get_or_insert_with(|| HookSpecificOutput {
                    hook_event_name: event_name(&event.kind).to_owned(),
                    ..HookSpecificOutput::default()
                });
                detail.permission_decision = Some("deny".to_owned());
                detail.permission_decision_reason = Some(reason.clone());
                2
            }
            // A stop boundary reads silence as "let the model stop". Dropping the context is part
            // of the encoding, not an oversight: an `additionalContext` here would continue the
            // conversation just as `decision: "block"` does.
            EventKind::TurnEnd { .. } | EventKind::SubagentEnd => {
                specific = None;
                0
            }
            _ => {
                output.decision = Some("block".to_owned());
                output.reason = Some(reason.clone());
                2
            }
        },
        HookDecision::Block { task } => {
            output.decision = Some("block".to_owned());
            output.reason = Some(task.clone());
            2
        }
        HookDecision::Halt => {
            output.continue_ = Some(false);
            0
        }
    };
    output.hook_specific_output = specific;
    let body = (output != HookOutput::default())
        .then(|| serde_json::to_value(output).expect("Claude hook output serialises"));
    VendorReply { body, exit_code }
}

fn event_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::SessionStart => "SessionStart",
        EventKind::SessionEnd => "SessionEnd",
        EventKind::PromptSubmit => "UserPromptSubmit",
        EventKind::PreTool { .. } => "PreToolUse",
        EventKind::PostTool { .. } => "PostToolUse",
        EventKind::TurnEnd { .. } => "Stop",
        EventKind::SubagentEnd => "SubagentStop",
        EventKind::Notification { .. } => "Notification",
        EventKind::CompactStart => "PreCompact",
        EventKind::CompactEnd => "PostCompact",
        EventKind::Usage { .. } | EventKind::HumanQuestion { .. } => "",
    }
}

/// Build the one settings file the host installs for Claude hooks.
///
/// A `hook_command` that is not an absolute UTF-8 path installs nothing, so the launch that names
/// the file cannot start.
pub(super) fn install(ctx: &AgentCtx<'_>, hook_command: &Path) -> HookInstall {
    let Some(hook_command) = hook_command
        .to_str()
        .filter(|_| hook_command.is_absolute())
        .map(str::to_owned)
    else {
        return HookInstall::default();
    };
    let hooks = HOOK_EVENTS
        .into_iter()
        .map(|event| {
            let handler = HookHandler {
                kind: "command".to_owned(),
                command: Some(hook_command.clone()),
                args: Some(Vec::new()),
                ..HookHandler::default()
            };
            let matcher = HookMatcher {
                hooks: vec![handler],
                ..HookMatcher::default()
            };
            (event.to_owned(), vec![matcher])
        })
        .collect::<BTreeMap<_, _>>();
    let settings = Settings {
        hooks,
        ..Settings::default()
    };
    let mut content = serde_json::to_string(&settings).expect("hook settings serialise");
    content.push('\n');

    HookInstall {
        files: vec![(settings_path(ctx), content)],
    }
}

#[cfg(test)]
mod tests {
    use dudes_proto::EventKind;
    use dudes_proto::reply::{HookDecision, HookReply, VendorReply};
    use dudes_types::{AgentId, Provenance, Timestamp};
    use serde_json::json;

    use super::{encode_reply, install, requires_reply, translate};
    use crate::adapter::AgentCtx;

    fn agent() -> AgentId {
        AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("an agent id")
    }

    /// A path the settings file cannot carry installs nothing, rather than panicking the caller:
    /// the launch that names the file then cannot start.
    #[test]
    fn an_unusable_hook_command_installs_nothing() {
        let agent_dir = std::path::Path::new("/var/lib/dudes/agents/01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let ctx = AgentCtx {
            extra_dirs: &[],
            agent: agent(),
            cwd: std::path::Path::new("/repo"),
            agent_dir,
            daemon_env: &[],
            sandbox: None,
            trusted: false,
            max_turns: None,
            max_budget: None,
            mcp_servers: &[],
        };
        let usable = std::path::Path::new(if cfg!(windows) {
            r"C:\dudes\bin\dudes-hook.exe"
        } else {
            "/opt/dudes/bin/dudes-hook"
        });
        assert_eq!(
            install(&ctx, usable).files.len(),
            1,
            "control: a usable path installs"
        );
        assert!(
            install(&ctx, std::path::Path::new("dudes-hook"))
                .files
                .is_empty()
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            let not_utf8 = std::ffi::OsStr::from_bytes(b"/opt/dudes/bin/dudes-\xffhook");
            assert!(
                install(&ctx, std::path::Path::new(not_utf8))
                    .files
                    .is_empty()
            );
        }
    }

    #[test]
    fn all_ten_hook_payloads_translate_and_preserve_raw() {
        let raws = [
            json!({"hook_event_name":"SessionStart","agent_id":"payload-selected"}),
            json!({"hook_event_name":"SessionEnd","reason":"other"}),
            json!({"hook_event_name":"UserPromptSubmit","prompt":"ship it"}),
            json!({"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cargo test"}}),
            json!({"hook_event_name":"PostToolUse","tool_name":"Edit","tool_response":{"ok":true}}),
            json!({"hook_event_name":"Stop","last_assistant_message":"done"}),
            json!({"hook_event_name":"SubagentStop"}),
            json!({"hook_event_name":"Notification","message":"permission needed"}),
            json!({"hook_event_name":"PreCompact","trigger":"auto"}),
            json!({"hook_event_name":"PostCompact","compact_summary":"summary"}),
        ];
        let expected = [
            EventKind::SessionStart,
            EventKind::SessionEnd,
            EventKind::PromptSubmit,
            EventKind::PreTool {
                name: "Bash".into(),
                input: json!({"command":"cargo test"}),
            },
            EventKind::PostTool {
                name: "Edit".into(),
                ok: true,
            },
            EventKind::TurnEnd {
                last_message: Some("done".into()),
            },
            EventKind::SubagentEnd,
            EventKind::Notification {
                text: "permission needed".into(),
            },
            EventKind::CompactStart,
            EventKind::CompactEnd,
        ];
        let at = Timestamp::from_unix_millis(1_789_000_000_000);
        let translated: Vec<_> = raws
            .iter()
            .map(|raw| translate(agent(), at, raw).expect("a modeled Claude event"))
            .collect();

        for ((event, raw), expected) in translated.iter().zip(&raws).zip(expected) {
            assert_eq!(event.agent, agent(), "payload identity is ignored");
            assert_eq!(event.at, at);
            assert_eq!(event.provenance, Provenance::Observed);
            assert_eq!(event.kind, expected);
            assert_eq!(event.raw.as_ref(), Some(raw), "vendor JSON stays verbatim");
        }
        let immediate: Vec<_> = translated
            .iter()
            .filter(|event| !requires_reply(event))
            .map(|event| &event.kind)
            .collect();
        assert_eq!(
            immediate,
            [
                &EventKind::SessionEnd,
                &EventKind::Notification {
                    text: "permission needed".into(),
                },
                &EventKind::CompactEnd,
            ],
            "exactly SessionEnd, Notification and PostCompact return immediately"
        );
    }

    #[test]
    fn hook_replies_keep_claudes_vendor_shapes() {
        let at = Timestamp::from_unix_millis(1_789_000_000_000);
        let pre_tool = translate(
            agent(),
            at,
            &json!({"hook_event_name":"PreToolUse","tool_name":"Bash"}),
        )
        .unwrap();
        assert_eq!(
            encode_reply(
                &HookReply {
                    decision: HookDecision::Deny {
                        reason: "the loop is paused".into(),
                    },
                    context: None,
                },
                &pre_tool,
            ),
            VendorReply {
                body: Some(json!({"hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": "the loop is paused"
                }})),
                exit_code: 2,
            }
        );

        let stop = translate(agent(), at, &json!({"hook_event_name":"Stop"})).unwrap();
        assert_eq!(
            encode_reply(
                &HookReply {
                    decision: HookDecision::Halt,
                    context: None,
                },
                &stop,
            ),
            VendorReply {
                body: Some(json!({"continue": false})),
                exit_code: 0,
            }
        );
        assert_eq!(
            encode_reply(
                &HookReply {
                    decision: HookDecision::Block {
                        task: "run the tests".into(),
                    },
                    context: None,
                },
                &stop,
            ),
            VendorReply {
                body: Some(json!({"decision":"block","reason":"run the tests"})),
                exit_code: 2,
            }
        );
        assert_eq!(
            encode_reply(&HookReply::ALLOW, &stop),
            VendorReply {
                body: None,
                exit_code: 0,
            }
        );
    }

    /// `Deny` and `Block` are opposite intents, so at a stop boundary they may never encode to
    /// an equal reply — a paused agent answering "keep working" is the defect this pins.
    ///
    /// The inequality alone is too weak to carry that, and both weaknesses are real mutants:
    /// swapping the two arms keeps the two unequal, and dropping `decision` while still exiting
    /// `2` keeps them unequal while the vendor still reads "keep working". So each boundary also
    /// asserts the absolute encoding on both sides, and a `Deny` carrying a context pins the
    /// third keep-working lever — `additionalContext` — staying unset as well. `PreToolUse` is
    /// the control: it separated the two before this fix and must still do so after it.
    #[test]
    fn deny_and_block_never_encode_equal_across_the_polarity_cells() {
        let at = Timestamp::from_unix_millis(1_789_000_000_000);
        let payload = "the loop is paused";
        let deny = |context: Option<&str>| HookReply {
            decision: HookDecision::Deny {
                reason: payload.to_owned(),
            },
            context: context.map(String::from),
        };
        let block = HookReply {
            decision: HookDecision::Block {
                task: payload.to_owned(),
            },
            context: None,
        };
        let silence = VendorReply {
            body: None,
            exit_code: 0,
        };
        let keep_working = VendorReply {
            body: Some(json!({"decision":"block","reason": payload})),
            exit_code: 2,
        };

        for raw in [
            json!({"hook_event_name":"Stop"}),
            json!({"hook_event_name":"SubagentStop"}),
        ] {
            let name = raw["hook_event_name"].as_str().expect("an event name");
            let event = translate(agent(), at, &raw).expect("a modeled Claude event");
            assert_ne!(
                encode_reply(&deny(None), &event),
                encode_reply(&block, &event),
                "{name}: a paused agent must not be told to keep working"
            );
            assert_eq!(
                encode_reply(&deny(None), &event),
                silence,
                "{name}: Deny writes no decision and does not exit 2"
            );
            assert_eq!(
                encode_reply(&deny(Some("a standing steer")), &event),
                silence,
                "{name}: Deny emits no additionalContext either"
            );
            assert_eq!(
                encode_reply(&block, &event),
                keep_working,
                "{name}: Block is the arm that must not move"
            );
        }

        let pre_tool = translate(
            agent(),
            at,
            &json!({"hook_event_name":"PreToolUse","tool_name":"Bash"}),
        )
        .expect("a modeled Claude event");
        assert_ne!(
            encode_reply(&deny(None), &pre_tool),
            encode_reply(&block, &pre_tool),
            "PreToolUse: the control that separated the two before this fix"
        );
        assert_eq!(
            encode_reply(&deny(None), &pre_tool),
            VendorReply {
                body: Some(json!({"hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": payload
                }})),
                exit_code: 2,
            },
            "PreToolUse: Deny stays a refusal here, not a silence"
        );
    }

    /// A paused agent's reply at `UserPromptSubmit`, pinned as it is encoded today: a top-level
    /// `decision: "block"` carrying the reason, with exit 2, which the vendor reads as "block and
    /// erase the prompt". This pins the cell a Floor pause of an attached session relies on; it
    /// decides nothing new. The other two encodings a
    /// `Deny` can take — the tool refusal and the stop boundary's silence — are the controls that
    /// it is neither.
    #[test]
    fn deny_at_prompt_submit_encodes_the_decided_cell() {
        let at = Timestamp::from_unix_millis(1_789_000_000_000);
        let paused = HookReply {
            decision: HookDecision::Deny {
                reason: "the loop is paused".to_owned(),
            },
            context: None,
        };
        let prompt = translate(
            agent(),
            at,
            &json!({"hook_event_name":"UserPromptSubmit","prompt":"ship it"}),
        )
        .expect("a modeled Claude event");
        assert_eq!(prompt.kind, EventKind::PromptSubmit);
        let encoded = encode_reply(&paused, &prompt);
        assert_eq!(
            encoded,
            VendorReply {
                body: Some(json!({"decision": "block", "reason": "the loop is paused"})),
                exit_code: 2,
            }
        );
        assert_ne!(
            encoded,
            VendorReply {
                body: None,
                exit_code: 0,
            },
            "not the stop boundary's silence"
        );
        assert!(
            encoded
                .body
                .as_ref()
                .is_some_and(|body| body.get("hookSpecificOutput").is_none()),
            "not the tool refusal's permissionDecision"
        );
    }
}
