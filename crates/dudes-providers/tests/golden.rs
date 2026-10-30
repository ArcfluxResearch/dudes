//! Golden tests over captured runs: `stream-json` output of Claude Code 2.1.239 and 2.1.283 (see
//! `fixtures/claude/README.md`) and `codex exec --json` output of codex-cli 0.158.0-alpha.2.1 (see
//! `fixtures/codex/README.md`). Each case is one whole run plus the exit code the process
//! returned; the expectations come from the capture, not from the parser.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use dudes_proto::ExitStatus;
use dudes_proto::providers::claude::Settings;
use dudes_providers::adapter::{Adapter, AgentCtx};
use dudes_providers::claude::outcome::{TurnOutcome, turn_outcome};
use dudes_providers::claude::usage::account;
use dudes_providers::claude::{ClaudeAdapter, Line, StreamLine, parse_line};
use dudes_providers::codex::stream::TurnStream;
use dudes_providers::stream::{
    LineRead, ScopedUsage, SessionObservation, TextBlock, TurnEnd, TurnReader,
};
use dudes_types::{AgentId, SessionId, Tokens, Usage, Usd};

// Forward-slash UNC paths are absolute under Windows and root-absolute under Unix, so the same
// literal fixture bytes exercise every supported platform.
const AGENT_DIR: &str = "//fixture.invalid/dudes-data/agents/01ARZ3NDEKTSV4RRFFQ69G5FAV";
const HOOK_COMMAND: &str = "//fixture.invalid/dudes/bin/dudes-hook";

fn claude_fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude")
}

fn claude_hook_events() -> [&'static str; 10] {
    [
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
    ]
}

fn hook_ctx() -> AgentCtx<'static> {
    AgentCtx {
        extra_dirs: &[],
        agent: AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
        cwd: Path::new("/tmp/dudes-work"),
        agent_dir: Path::new(AGENT_DIR),
        daemon_env: &[],
        sandbox: None,
        trusted: false,
        max_turns: None,
        max_budget: None,
        mcp_servers: &[],
    }
}

fn hook_contract(
    text: &str,
    hook_command: &Path,
    hook_events: &[&str],
) -> Result<Settings, String> {
    let settings: Settings = serde_json::from_str(text)
        .map_err(|error| format!("settings must be valid JSON: {error}"))?;
    let expected: BTreeSet<&str> = hook_events.iter().copied().collect();
    let actual: BTreeSet<&str> = settings.hooks.keys().map(String::as_str).collect();
    let mut errors = Vec::new();

    for event in expected.difference(&actual) {
        errors.push(format!("missing event `{event}`"));
    }
    for event in actual.difference(&expected) {
        errors.push(format!("unexpected event `{event}`"));
    }
    if settings.hooks.is_empty() {
        errors.push("hook event map must not be empty".to_owned());
    }
    if settings.hooks.len() != hook_events.len() {
        errors.push(format!(
            "hook event count must be {}, got {}",
            hook_events.len(),
            settings.hooks.len()
        ));
    }
    if !settings.extra.is_empty() {
        errors.push("hook settings must contain no non-hook keys".to_owned());
    }

    let command = hook_command.to_str().expect("the test command is UTF-8");
    for &event in hook_events {
        let Some(matchers) = settings.hooks.get(event) else {
            continue;
        };
        if matchers.len() != 1 {
            errors.push(format!("`{event}` must have exactly one matcher group"));
            continue;
        }
        let matcher = &matchers[0];
        if matcher.matcher.is_some() {
            errors.push(format!("`{event}` matcher must be absent"));
        }
        if matcher.hooks.len() != 1 {
            errors.push(format!("`{event}` must have exactly one handler"));
            continue;
        }
        let handler = &matcher.hooks[0];
        if handler.kind != "command" {
            errors.push(format!(
                "`{event}` handler type must be `command`; HTTP handlers are forbidden"
            ));
        }
        if handler.command.as_deref() != Some(command) {
            errors.push(format!(
                "`{event}` command must equal the supplied absolute path; shell wrappers are forbidden"
            ));
        }
        if handler.args.as_deref() != Some(&[]) {
            errors.push(format!("`{event}` args must be present and empty"));
        }
        if handler.url.is_some() {
            errors.push(format!("`{event}` url must be absent"));
        }
        if handler.timeout.is_some() {
            errors.push(format!("`{event}` timeout must be absent"));
        }
        if handler
            .command
            .iter()
            .chain(handler.args.iter().flatten())
            .any(|part| part.contains("DUDES_SOCK") || part.contains("DUDES_TOKEN"))
        {
            errors.push(format!("`{event}` command argv must contain no credential"));
        }
    }

    if errors.is_empty() {
        Ok(settings)
    } else {
        Err(errors.join("; "))
    }
}

#[test]
fn hook_install_golden_has_exactly_ten_exec_events() {
    let adapter: &dyn Adapter = &ClaudeAdapter::default();
    let ctx = hook_ctx();
    let fixture_dir = claude_fixture_dir();
    let hook_events = claude_hook_events();
    for (name, path) in [
        ("agent directory", Path::new(AGENT_DIR)),
        ("hook command", Path::new(HOOK_COMMAND)),
    ] {
        assert!(
            path.is_absolute(),
            "{name} fixture must be absolute on this platform: {}",
            path.display()
        );
    }
    let install = adapter.install_hooks(&ctx, Path::new(HOOK_COMMAND));
    let regenerated = adapter.install_hooks(&ctx, Path::new(HOOK_COMMAND));
    let expected = fs::read_to_string(fixture_dir.join("hook-install.json"))
        .expect("hook-install golden exists");

    assert_eq!(
        install, regenerated,
        "equal inputs must regenerate equal bytes"
    );
    assert_eq!(
        install.files,
        vec![(
            Path::new(AGENT_DIR).join("hooks/settings.json"),
            expected.clone()
        )]
    );
    let settings = hook_contract(&expected, Path::new(HOOK_COMMAND), &hook_events)
        .unwrap_or_else(|error| panic!("positive hook golden violated the contract: {error}"));
    assert_ne!(settings.hooks.len(), 0, "the event set must be non-empty");
    assert_eq!(settings.hooks.len(), 10, "the exact event count");
    assert_eq!(
        settings
            .hooks
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        hook_events.into_iter().collect()
    );
}

#[test]
fn hook_install_negative_goldens_are_rejected() {
    let fixture_dir = claude_fixture_dir();
    let hook_events = claude_hook_events();
    let rejected = |fixture: &str| {
        let text = fs::read_to_string(fixture_dir.join(fixture))
            .unwrap_or_else(|error| panic!("{fixture}: {error}"));
        match hook_contract(&text, Path::new(HOOK_COMMAND), &hook_events) {
            Ok(_) => panic!("{fixture}: invalid snapshot passed the hook contract"),
            Err(error) => error,
        }
    };

    let http = rejected("hook-install-http.json");
    assert!(
        http.contains("HTTP handlers are forbidden"),
        "HTTP fixture failed for the wrong reason: {http}"
    );

    let shell = rejected("hook-install-shell.json");
    assert!(
        shell.contains("shell wrappers are forbidden"),
        "shell fixture failed for the wrong reason: {shell}"
    );
    assert!(
        shell.contains("args must be present and empty"),
        "shell fixture did not prove missing args are rejected: {shell}"
    );

    let missing = rejected("hook-install-missing-event.json");
    assert!(
        missing.contains("missing event `PostCompact`"),
        "missing-event fixture failed for the wrong reason: {missing}"
    );
}

fn exit(code: i32) -> ExitStatus {
    ExitStatus {
        code: Some(code),
        signal: None,
    }
}

/// One captured run: the fixture, how the process exited, and what the adapter must conclude.
struct Case {
    fixture: &'static str,
    exit: i32,
    outcome: TurnOutcome,
    /// What the supervisor journals on `TurnEnded.outcome` for this run.
    wire: dudes_proto::TurnOutcome,
    /// Micro-dollars from `total_cost_usd`.
    usd_micros: u64,
    /// Output tokens summed over `modelUsage`.
    output_tokens: u64,
    /// Cache-read tokens summed over `modelUsage`.
    cache_read: u64,
    /// `type` (or `type/subtype`) of every line, consecutive duplicates collapsed.
    shape: &'static [&'static str],
}

#[allow(clippy::too_many_lines)] // one entry per captured run; a table, not logic
fn cases() -> Vec<Case> {
    vec![
        Case {
            fixture: "r1-hooks",
            exit: 0,
            outcome: TurnOutcome::Completed {
                last_message: Some("done".into()),
            },
            wire: dudes_proto::TurnOutcome::Completed,
            usd_micros: 31_575,
            output_tokens: 226,
            cache_read: 48_810,
            shape: &[
                "system/hook_started",
                "system/hook_response",
                "system/hook_progress",
                "system/hook_response",
                "system/init",
                "system/thinking_tokens",
                "assistant",
                "rate_limit_event",
                "user",
                "system/thinking_tokens",
                "assistant",
                "result/success",
            ],
        },
        Case {
            fixture: "r4-budget",
            exit: 1,
            outcome: TurnOutcome::BudgetExhausted,
            wire: dudes_proto::TurnOutcome::BudgetExhausted,
            usd_micros: 19_930,
            output_tokens: 176,
            cache_read: 22_317,
            shape: &[
                "system/hook_started",
                "system/hook_response",
                "system/hook_progress",
                "system/hook_response",
                "system/init",
                "system/thinking_tokens",
                "assistant",
                "result/error_max_budget_usd",
            ],
        },
        Case {
            fixture: "r5-maxturns",
            exit: 1,
            outcome: TurnOutcome::MaxTurns,
            wire: dudes_proto::TurnOutcome::MaxTurns,
            usd_micros: 20_286,
            output_tokens: 258,
            cache_read: 22_317,
            shape: &[
                "system/hook_started",
                "system/hook_response",
                "system/hook_progress",
                "system/hook_response",
                "system/init",
                "system/thinking_tokens",
                "assistant",
                "rate_limit_event",
                "user",
                "result/error_max_turns",
            ],
        },
        Case {
            fixture: "r7-bare",
            exit: 1,
            outcome: TurnOutcome::Failed {
                reason: "api_error: Not logged in · Please run /login".into(),
            },
            wire: dudes_proto::TurnOutcome::Failed {
                reason: "api_error: Not logged in · Please run /login".into(),
            },
            usd_micros: 0,
            output_tokens: 0,
            cache_read: 0,
            shape: &["system/init", "assistant", "result/success"],
        },
        Case {
            fixture: "r8-stopblock",
            exit: 0,
            outcome: TurnOutcome::Completed {
                last_message: Some("ping".into()),
            },
            wire: dudes_proto::TurnOutcome::Completed,
            usd_micros: 23_620,
            output_tokens: 265,
            cache_read: 52_894,
            shape: &[
                "system/hook_started",
                "system/hook_response",
                "system/hook_progress",
                "system/hook_response",
                "system/init",
                "system/thinking_tokens",
                "assistant",
                "rate_limit_event",
                "user",
                "system/notification",
                "system/thinking_tokens",
                "assistant",
                "result/success",
            ],
        },
        Case {
            fixture: "s1-first",
            exit: 0,
            outcome: TurnOutcome::Completed {
                last_message: Some("OK".into()),
            },
            wire: dudes_proto::TurnOutcome::Completed,
            usd_micros: 33_243,
            output_tokens: 4,
            cache_read: 11_095,
            shape: &[
                "system/init",
                "assistant",
                "rate_limit_event",
                "result/success",
            ],
        },
        Case {
            fixture: "s2-resume",
            exit: 0,
            outcome: TurnOutcome::Completed {
                last_message: Some("OK".into()),
            },
            wire: dudes_proto::TurnOutcome::Completed,
            usd_micros: 66_990,
            output_tokens: 8,
            cache_read: 22_190,
            shape: &[
                "system/init",
                "assistant",
                "rate_limit_event",
                "result/success",
            ],
        },
        Case {
            fixture: "s3-resume",
            exit: 0,
            outcome: TurnOutcome::Completed {
                last_message: Some("OK".into()),
            },
            wire: dudes_proto::TurnOutcome::Completed,
            usd_micros: 101_241,
            output_tokens: 12,
            cache_read: 33_285,
            shape: &[
                "system/init",
                "assistant",
                "rate_limit_event",
                "result/success",
            ],
        },
    ]
}

fn read(fixture_dir: &Path, fixture: &str) -> String {
    fs::read_to_string(fixture_dir.join(format!("{fixture}.stdout"))).expect("fixture exists")
}

fn parse_all<T, E>(
    fixture_dir: &Path,
    fixture: &str,
    mut parse: impl FnMut(&str) -> Result<T, E>,
) -> Vec<T>
where
    E: std::fmt::Display,
{
    read(fixture_dir, fixture)
        .lines()
        .enumerate()
        .map(|(n, text)| parse(text).unwrap_or_else(|e| panic!("{fixture}:{}: {e}", n + 1)))
        .collect()
}

fn shape_of<T>(lines: &[T], mut tag_of: impl FnMut(&T) -> String) -> Vec<String> {
    let mut shape: Vec<String> = Vec::new();
    for line in lines {
        let tag = tag_of(line);
        if shape.last() != Some(&tag) {
            shape.push(tag);
        }
    }
    shape
}

fn claude_tag(line: &Line) -> String {
    match &line.envelope.subtype {
        Some(sub) => format!("{}/{sub}", line.envelope.kind),
        None => line.envelope.kind.clone(),
    }
}

/// Every line of every captured run parses, the line sequence is the recorded
/// one, and every line carries the session announced by `system/init`.
#[test]
fn every_fixture_parses_to_its_recorded_shape() {
    let fixture_dir = claude_fixture_dir();
    for case in cases() {
        let lines = parse_all(&fixture_dir, case.fixture, parse_line);
        assert_eq!(shape_of(&lines, claude_tag), case.shape, "{}", case.fixture);
        let init = lines
            .iter()
            .find_map(|line| match &line.body {
                StreamLine::Init(init) => Some(init),
                _ => None,
            })
            .expect("an init line");
        for line in &lines {
            assert_eq!(
                line.session_id(),
                Some(init.session_id.as_str()),
                "{}",
                case.fixture
            );
        }

        let raw_plugins = read(&fixture_dir, case.fixture)
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|line| line.get("subtype") == Some(&serde_json::json!("init")))
            .and_then(|line| line.get("plugins").cloned())
            .expect("each captured init carries plugins");
        assert_eq!(
            serde_json::Value::Array(init.plugins.clone()),
            raw_plugins,
            "{}: plugins parse without imposing a vendor-owned shape",
            case.fixture
        );
    }
}

/// The outcome follows from `(subtype, is_error, terminal_reason, exit
/// code)` alone — `success` + `is_error` (r7) is a failure, and the error-arm runs (r4, r5, r7)
/// close to a definite outcome without any hook event, because `turn_outcome` has no parameter
/// through which one could be supplied.
#[test]
fn outcome_follows_from_result_and_exit_only() {
    let fixture_dir = claude_fixture_dir();
    for case in cases() {
        let lines = parse_all(&fixture_dir, case.fixture, parse_line);
        let result = lines.iter().find_map(|line| match &line.body {
            StreamLine::Result(result) => Some(result),
            _ => None,
        });
        assert_eq!(
            turn_outcome(result, exit(case.exit)),
            case.outcome,
            "{}",
            case.fixture
        );
    }
}

/// the outcome each captured run crosses to on the wire, which is what
/// `Persist{TurnEnded}` carries. `last_message` and `exit` are dropped in the crossing because
/// `Record::TurnEnded` holds them as fields of its own; `Failed.reason` — here the vendor's own
/// words, captured in r7 — is kept, bounded at `MAX_TEXT_BYTES` by the crossing, which leaves
/// every captured reason unchanged because all of them are far under it.
///
/// One of the five variants has no capture: every run recorded here wrote a `result` line,
/// whatever its exit code, so `no_result` is *derived* from a capture rather than captured —
/// r5's own stream cut at the line before its `result`, which is the condition the variant
/// names. The derivation is marked as such below; nothing else in this file is derived.
#[test]
fn every_fixture_crosses_to_its_wire_outcome() {
    let fixture_dir = claude_fixture_dir();
    let tag = |wire: &dudes_proto::TurnOutcome| {
        serde_json::to_value(wire).expect("wire outcome serialises")["kind"]
            .as_str()
            .expect("a `kind` tag")
            .to_owned()
    };
    let mut seen: Vec<String> = Vec::new();
    for case in cases() {
        assert_eq!(
            dudes_proto::TurnOutcome::from(case.outcome.clone()),
            case.wire,
            "{}",
            case.fixture
        );
        seen.push(tag(&case.wire));
    }

    // Derived, not captured: r5's stream up to but not including its `result` line.
    let lines = parse_all(&fixture_dir, "r5-maxturns", parse_line);
    let cut = lines
        .iter()
        .position(|line| matches!(line.body, StreamLine::Result(_)))
        .expect("a result line to cut at");
    let truncated = &lines[..cut];
    // The `None` is *found* by the same `find_map` the whole-run test above uses, rather than
    // written down — which is what makes the assert below able to fail at all.
    //
    // Be precise about what it guards: the slice, not the fixture. `position` returns the
    // first `result` line and `..cut` excludes it and everything after, so an off-by-one is
    // the only way a result line survives into `truncated` — `..=cut` reddens this line. It
    // says nothing about a fixture growing a *second* `result` line: that line would sit
    // beyond `cut` and never be looked at here. `every_fixture_parses_to_its_recorded_shape`
    // is what covers a fixture's contents.
    let result = truncated.iter().find_map(|line| match &line.body {
        StreamLine::Result(result) => Some(result),
        _ => None,
    });
    assert!(result.is_none(), "the slice kept a result line");
    let outcome = turn_outcome(result, exit(1));
    assert_eq!(outcome, TurnOutcome::NoResult { exit: exit(1) });
    let wire = dudes_proto::TurnOutcome::from(outcome);
    assert_eq!(wire, dudes_proto::TurnOutcome::NoResult);
    seen.push(tag(&wire));

    // The five outcomes captured runs can reach, and only those: `killed` and `vanished` are the supervisor's
    // and the reaper's, and no captured run can reach them.
    seen.sort();
    seen.dedup();
    assert_eq!(
        seen,
        [
            "budget_exhausted",
            "completed",
            "failed",
            "max_turns",
            "no_result"
        ]
    );
}

/// Usage comes from `modelUsage` and `total_cost_usd`. On r4 the vendor's
/// `usage` block is all zero while the real numbers are in `modelUsage`; the account must show
/// them.
#[test]
fn usage_is_accounted_from_model_usage_and_total_cost() {
    let fixture_dir = claude_fixture_dir();
    for case in cases() {
        let lines = parse_all(&fixture_dir, case.fixture, parse_line);
        let result = lines
            .iter()
            .find_map(|line| match &line.body {
                StreamLine::Result(result) => Some(result),
                _ => None,
            })
            .expect("a result line");
        let usage = account(result).expect("valid cost");
        assert_eq!(
            usage.usd.map(Usd::micros),
            Some(case.usd_micros),
            "{}",
            case.fixture
        );
        assert_eq!(usage.output.count(), case.output_tokens, "{}", case.fixture);
        assert_eq!(
            usage.cache_read.count(),
            case.cache_read,
            "{}",
            case.fixture
        );
    }
}

/// Fixture hygiene: the captures were normalised before they were committed (see the README);
/// no line may carry an operator's filesystem path. Also guards the rule that CI fails
/// on an empty fixture directory.
#[test]
fn fixtures_carry_no_operator_paths() {
    let fixture_dir = claude_fixture_dir();
    assert_no_operator_paths(
        &fixture_dir,
        "stdout",
        cases().len(),
        &[concat!("/Use", "rs/"), "/private/tmp", "/home/"],
    );
}

fn assert_no_operator_paths(
    fixture_dir: &Path,
    extension: &str,
    expected_count: usize,
    needles: &[&str],
) {
    let entries: Vec<_> = fs::read_dir(fixture_dir)
        .expect("fixture dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|ext| ext == extension))
        .collect();
    assert_eq!(entries.len(), expected_count, "one fixture per case");
    for path in entries {
        let text = fs::read_to_string(&path).expect("readable");
        for (n, line) in text.lines().enumerate() {
            for needle in needles {
                assert!(
                    !line.contains(needle),
                    "{}:{}: contains `{needle}`",
                    path.display(),
                    n + 1
                );
            }
        }
    }
}

/// The recorded runs cover a `tool_use` block (and its `tool_result`), so no
/// new recording is needed for the adapter to be proven against a tool-calling turn.
#[test]
fn a_recorded_fixture_carries_a_tool_use_block() {
    let fixture_dir = claude_fixture_dir();
    let kinds = |fixture: &str, want: &str| {
        parse_all(&fixture_dir, fixture, parse_line)
            .iter()
            .any(|line| match &line.body {
                StreamLine::Assistant(msg) | StreamLine::User(msg) => {
                    msg.message.content.iter().any(|block| block.kind == want)
                }
                _ => false,
            })
    };
    let with_tool_use: Vec<_> = cases()
        .iter()
        .map(|case| case.fixture)
        .filter(|fixture| kinds(fixture, "tool_use") && kinds(fixture, "tool_result"))
        .collect();
    assert!(
        !with_tool_use.is_empty(),
        "record a run with a tool call before pinning the adapter"
    );
}

fn codex_fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex")
}

/// One captured Codex run and what its reader must conclude. Every run ended `Completed`.
struct CodexCase {
    fixture: &'static str,
    exit: i32,
    thread: &'static str,
    text: &'static [&'static str],
    reply: &'static str,
    /// `turn.completed.usage` with the cache taken out of the input.
    usage: Usage,
}

const fn tokens(input: u64, cache_read: u64, output: u64) -> Usage {
    Usage {
        input: Tokens::new(input),
        output: Tokens::new(output),
        cache_read: Tokens::new(cache_read),
        ..Usage::ZERO
    }
}

fn codex_cases() -> [CodexCase; 4] {
    [
        CodexCase {
            fixture: "c1-success",
            exit: 0,
            thread: "01a0e1d9-3860-7800-a9ff-2994ef3b1c6f",
            text: &["ok"],
            reply: "ok",
            usage: tokens(3865, 12_288, 5),
        },
        CodexCase {
            fixture: "c2-command",
            exit: 0,
            thread: "01a0e1d9-84d5-7bc2-8567-d32735103d71",
            text: &["I\u{2019}ll run `echo ok`.\n", "\nok"],
            reply: "ok",
            usage: tokens(4106, 28_288, 52),
        },
        CodexCase {
            fixture: "c3-resume",
            exit: 0,
            thread: "01a0e1d9-3860-7800-a9ff-2994ef3b1c6f",
            text: &["ok"],
            reply: "ok",
            usage: tokens(4035, 28_288, 10),
        },
        CodexCase {
            fixture: "c4-file-change",
            exit: 0,
            thread: "00000000-0000-7000-8000-000000000001",
            text: &[
                "I\u{2019}ll follow the requested order and make the two file changes.\n",
                "\ndone",
            ],
            reply: "done",
            usage: tokens(5224, 77_184, 350),
        },
    ]
}

fn read_codex(lines: &[String], exit_code: i32) -> (Vec<LineRead>, TurnEnd) {
    let mut reader: Box<dyn TurnReader> = Box::new(TurnStream::new());
    let reads = lines.iter().map(|line| reader.line(line)).collect();
    (reads, reader.end(exit(exit_code)))
}

fn codex_lines(fixture: &str) -> Vec<String> {
    read(&codex_fixture_dir(), fixture)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Codex names its thread on the first line, so the session opens there and on no other line.
#[test]
fn every_codex_fixture_reads_to_its_recorded_end() {
    for case in codex_cases() {
        let (reads, end) = read_codex(&codex_lines(case.fixture), case.exit);
        let thread = SessionId::new(case.thread);
        let opened: Vec<(usize, &SessionObservation)> = reads
            .iter()
            .enumerate()
            .filter_map(|(at, read)| read.session.as_ref().map(|session| (at, session)))
            .collect();
        assert_eq!(
            opened,
            [(
                0,
                &SessionObservation {
                    session: Some(thread.clone()),
                    ..SessionObservation::default()
                }
            )],
            "{}",
            case.fixture
        );
        let text: Vec<TextBlock> = reads.into_iter().flat_map(|read| read.text).collect();
        let expected: Vec<TextBlock> = case
            .text
            .iter()
            .map(|text| TextBlock {
                session: thread.clone(),
                text: (*text).to_owned(),
            })
            .collect();
        assert_eq!(text, expected, "{}", case.fixture);

        assert_eq!(
            end.outcome,
            dudes_proto::TurnOutcome::Completed,
            "{}",
            case.fixture
        );
        assert_eq!(
            end.last_message.as_deref(),
            Some(case.reply),
            "{}",
            case.fixture
        );
        assert_eq!(
            end.usage,
            Some(ScopedUsage::Session {
                total: case.usage,
                calls: Usage::ZERO,
            }),
            "{}",
            case.fixture
        );
        assert_eq!(end.session, Some(thread), "{}", case.fixture);
        assert!(end.model_usage.is_empty(), "{}", case.fixture);
        assert!(end.usage_refused.is_none(), "{}", case.fixture);
        assert_eq!(end.attribution_refused, 0, "{}", case.fixture);
        assert_eq!(end.unreadable, 0, "{}", case.fixture);
    }
}

#[test]
fn a_resumed_codex_thread_reports_the_original_session() {
    let original = read_codex(&codex_lines("c1-success"), 0).1.session;
    let resumed = read_codex(&codex_lines("c3-resume"), 0).1.session;
    assert!(original.is_some(), "the original run names its thread");
    assert_eq!(resumed, original);
}

/// Derived from `c1-success`, not captured: every captured run records a `usage`.
#[test]
fn a_codex_turn_completed_without_usage_accounts_nothing() {
    let captured = codex_lines("c1-success");
    let stripped: Vec<String> = captured
        .iter()
        .map(|line| {
            let mut event: serde_json::Value = serde_json::from_str(line).unwrap();
            if event["type"] == "turn.completed" {
                event.as_object_mut().unwrap().remove("usage").unwrap();
                return event.to_string();
            }
            line.clone()
        })
        .collect();
    assert_ne!(stripped, captured, "the usage key was really removed");

    let end = read_codex(&stripped, 0).1;
    assert_eq!(end.usage, None);
    assert_eq!(end.outcome, dudes_proto::TurnOutcome::Completed);
    assert_eq!(end.last_message.as_deref(), Some("ok"));

    let control = read_codex(&captured, 0).1;
    assert!(
        matches!(control.usage, Some(ScopedUsage::Session { .. })),
        "control: the captured line accounts its usage"
    );
}

#[test]
fn codex_fixtures_carry_no_operator_paths() {
    assert_no_operator_paths(
        &codex_fixture_dir(),
        "stdout",
        codex_cases().len(),
        &[concat!("/Use", "rs/"), "/private/tmp", "/home/"],
    );
}
