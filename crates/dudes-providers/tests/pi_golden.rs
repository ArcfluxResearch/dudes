//! Golden runs of `pi --mode json`, read through the Pi turn reader (see `fixtures/pi/README.md`).
//! Each case is one whole run, the exit code the process returned, and what the reader must
//! conclude.

use std::fs;
use std::path::Path;

use dudes_proto::{ExitStatus, TurnOutcome};
use dudes_providers::pi::stream::TurnStream;
use dudes_providers::steps::{StepEvent, StepKind};
use dudes_providers::stream::{LineRead, ScopedUsage, TurnEnd, TurnReader};
use dudes_types::{SessionId, Usage};

fn fixture(name: &str) -> String {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pi")
            .join(name),
    )
    .unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn read_lines<'a>(lines: impl IntoIterator<Item = &'a str>, code: i32) -> (Vec<LineRead>, TurnEnd) {
    let mut reader: Box<dyn TurnReader> = Box::new(TurnStream::new());
    let reads = lines.into_iter().map(|line| reader.line(line)).collect();
    let end = reader.end(ExitStatus {
        code: Some(code),
        signal: None,
    });
    (reads, end)
}

fn read(name: &str) -> (Vec<LineRead>, TurnEnd) {
    read_lines(fixture(name).lines(), 0)
}

/// The turn's own usage. Pi reports no session running total, so a `Session` figure fails.
fn turn_usage(end: &TurnEnd) -> Option<Usage> {
    end.usage.map(|scoped| match scoped {
        ScopedUsage::Turn(usage) => usage,
        ScopedUsage::Session { total, .. } => panic!("Pi reported a session total: {total:?}"),
    })
}

/// Input, output, cache read and cache write, in that order.
fn tokens(end: &TurnEnd) -> Option<[u64; 4]> {
    turn_usage(end).map(|usage| {
        [
            usage.input.count(),
            usage.output.count(),
            usage.cache_read.count(),
            usage.cache_creation.count(),
        ]
    })
}

struct Case {
    fixture: &'static str,
    session: &'static str,
    outcome: TurnOutcome,
    last_message: Option<&'static str>,
    tokens: Option<[u64; 4]>,
    models: &'static [&'static str],
}

fn cases() -> Vec<Case> {
    let mut cases = vec![
        Case {
            fixture: "l1-success.jsonl",
            session: "f0e58032-8233-758b-afd1-7b4fc923b785",
            outcome: TurnOutcome::Completed,
            last_message: Some("OK"),
            tokens: Some([578, 5, 0, 0]),
            models: &["gpt-5.5"],
        },
        Case {
            fixture: "l2-tool.jsonl",
            session: "a6eec217-e4e9-7c84-86a4-8b739a45e24d",
            outcome: TurnOutcome::Completed,
            last_message: Some("TANGERINE"),
            tokens: Some([592 + 628, 18 + 7, 0, 0]),
            models: &["gpt-5.5"],
        },
        Case {
            fixture: "l3-turn1.jsonl",
            session: "3d8f310f-9cb5-7185-a385-54c0378f2355",
            outcome: TurnOutcome::Completed,
            last_message: Some("Got it \u{2014} PLUM."),
            tokens: Some([578, 10, 0, 0]),
            models: &["gpt-5.5"],
        },
        Case {
            fixture: "l3-turn2.jsonl",
            session: "3d8f310f-9cb5-7185-a385-54c0378f2355",
            outcome: TurnOutcome::Completed,
            last_message: Some("PLUM"),
            tokens: Some([603, 6, 0, 0]),
            models: &["gpt-5.5"],
        },
        Case {
            fixture: "h1-retry.jsonl",
            session: "7fdc1d8e-02bd-711c-8c85-11a32c6372ba",
            outcome: TurnOutcome::Completed,
            last_message: Some("stub reply 2"),
            tokens: Some([10, 3, 0, 0]),
            models: &["stub-1"],
        },
        Case {
            fixture: "c1-compaction.jsonl",
            session: "0a080528-23b0-7cbe-9d2e-80780fd83ddd",
            outcome: TurnOutcome::Completed,
            last_message: Some("stub reply 1"),
            tokens: Some([11 + 22, 3 + 3, 0, 0]),
            models: &["stub-1"],
        },
        Case {
            fixture: "e1-login-expired.jsonl",
            session: "2439c96e-3d6b-7654-a096-3f4fa43a1bc2",
            outcome: TurnOutcome::Failed {
                reason: concat!(
                    "Pi reported an error: OAuth refresh failed for openai-codex: OpenAI Codex ",
                    "token refresh failed (401): Your session has ended. Please log in again. ",
                    "(refresh_token_invalidated)"
                )
                .to_owned(),
            },
            last_message: None,
            tokens: None,
            models: &[],
        },
        Case {
            fixture: "p1-model-error.jsonl",
            session: "01a17058-f280-7101-9040-058259d463c7",
            outcome: TurnOutcome::Failed {
                reason: concat!(
                    "Pi reported an error: OpenAI API error (400): The 'gpt-5.5' model is not ",
                    "supported when using Codex with a ChatGPT account. (model_not_found)"
                )
                .to_owned(),
            },
            last_message: None,
            tokens: None,
            models: &[],
        },
        Case {
            fixture: "p2-bash-tool.jsonl",
            session: "01a1705b-b2db-75ad-bd5f-256c3ebda947",
            outcome: TurnOutcome::Completed,
            last_message: Some("there are 3 files"),
            tokens: Some([1255 + 146, 22 + 49, 1152, 0]),
            models: &["gpt-6.1-sol"],
        },
    ];
    cases.extend(pi_v1_cases());
    cases
}

fn pi_v1_cases() -> [Case; 7] {
    [
        Case {
            fixture: "v1-l1-success.jsonl",
            session: "044903d3-7af5-7b89-8640-7577cdedbf56",
            outcome: TurnOutcome::Completed,
            last_message: Some("stub reply 1"),
            tokens: Some([11, 3, 0, 0]),
            models: &["stub-1"],
        },
        Case {
            fixture: "v1-l2-tool.jsonl",
            session: "b8917449-c647-7358-bf7e-68e9dee2fae7",
            outcome: TurnOutcome::Completed,
            last_message: Some("stub reply 2"),
            tokens: Some([33, 6, 0, 0]),
            models: &["stub-1"],
        },
        Case {
            fixture: "v1-l3-turn1.jsonl",
            session: "a78d4536-c0a7-74bc-b955-3dc4b5ef3c7c",
            outcome: TurnOutcome::Completed,
            last_message: Some("stub reply 1"),
            tokens: Some([11, 3, 0, 0]),
            models: &["stub-1"],
        },
        Case {
            fixture: "v1-l3-turn2.jsonl",
            session: "a78d4536-c0a7-74bc-b955-3dc4b5ef3c7c",
            outcome: TurnOutcome::Completed,
            last_message: Some("stub reply 2"),
            tokens: Some([22, 3, 0, 0]),
            models: &["stub-1"],
        },
        Case {
            fixture: "v1-h1-retry.jsonl",
            session: "0e0995be-75f6-7e32-9cd8-1b476129c855",
            outcome: TurnOutcome::Completed,
            last_message: Some("stub reply 2"),
            tokens: Some([22, 3, 0, 0]),
            models: &["stub-1"],
        },
        Case {
            fixture: "v1-c1-compaction.jsonl",
            session: "a35ac199-0db1-7773-b2fb-7a9beae47f6a",
            outcome: TurnOutcome::Completed,
            last_message: Some("stub reply 1"),
            tokens: Some([33, 6, 0, 0]),
            models: &["stub-1"],
        },
        Case {
            fixture: "v1-e1-failed.jsonl",
            session: "14958ae9-0c30-7f42-bfd3-f08b68b2de7b",
            outcome: TurnOutcome::Failed {
                reason: "Pi reported an error: 401: probe sink: unauthorized".to_owned(),
            },
            last_message: None,
            tokens: None,
            models: &[],
        },
    ]
}

#[test]
fn pi_v1_retry_entry_appended_does_not_end_or_charge_the_turn() {
    let text = fixture("v1-h1-retry.jsonl");
    let lines: Vec<_> = text.lines().collect();
    let appended = lines
        .iter()
        .position(|line| line.contains(r#""type":"entry_appended""#))
        .expect("1.0.4 persists its retry entry");
    let (_, cut) = read_lines(lines[..=appended].iter().copied(), 0);
    assert_eq!(cut.outcome, TurnOutcome::NoResult);
    assert_eq!(tokens(&cut), None);
    assert_eq!(tokens(&read("v1-h1-retry.jsonl").1), Some([22, 3, 0, 0]));
}

#[test]
fn every_run_reads_to_its_recorded_end() {
    for case in cases() {
        let (_, end) = read(case.fixture);
        assert_eq!(end.outcome, case.outcome, "{}", case.fixture);
        assert_eq!(
            end.last_message.as_deref(),
            case.last_message,
            "{}",
            case.fixture
        );
        assert_eq!(tokens(&end), case.tokens, "{}", case.fixture);
        assert_eq!(
            turn_usage(&end).and_then(|usage| usage.usd),
            None,
            "{}: cost is not charged",
            case.fixture
        );
        assert_eq!(
            end.model_usage
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            case.models,
            "{}",
            case.fixture
        );
        assert_eq!(
            end.session.as_ref().map(SessionId::as_str),
            Some(case.session),
            "{}",
            case.fixture
        );
        assert_eq!(
            end.unreadable, 0,
            "{}: a payload shape drifted",
            case.fixture
        );
    }
}

/// Pi 1.4.0 emits a system message and several `message_update` and `tool_execution_update` lines
/// around one `bash` call. The call opens once and closes once, without failing.
#[test]
fn a_pi_1_4_bash_call_is_one_command_step_that_ends_done() {
    let (reads, _) = read("p2-bash-tool.jsonl");
    let steps: Vec<_> = reads.iter().flat_map(|read| &read.steps).collect();
    let [
        StepEvent::Open { step, .. },
        StepEvent::Close { id, failed, .. },
    ] = steps[..]
    else {
        panic!("expected one open and one close, got {steps:?}");
    };
    assert_eq!((step.kind, step.tool.as_str()), (StepKind::Command, "bash"));
    assert_eq!(&step.id, id);
    assert!(!failed);
}

/// Pi 1.4.0 reports a rejected model as an assistant message with `stopReason` "error" and exits 0.
/// Its own words reach the turn's reason and its reply, which is what a Chat page shows.
#[test]
fn a_model_the_account_cannot_use_fails_the_turn_in_pi_s_words() {
    let (_, end) = read("p1-model-error.jsonl");
    let said = "Pi reported an error: OpenAI API error (400): The 'gpt-5.5' model is not supported \
        when using Codex with a ChatGPT account. (model_not_found)";
    assert_eq!(
        end.outcome,
        TurnOutcome::Failed {
            reason: said.to_owned()
        }
    );
    assert_eq!(
        end.final_reply.map(|reply| reply.text).as_deref(),
        Some(said)
    );
    assert_eq!(end.last_message, None);
}

/// With an ambiguous `--model`, Pi prints one line to standard error, writes no JSON and exits 1.
/// The terminal merges the two streams, so the line reaches the reader as an unreadable line.
#[test]
fn a_start_up_error_on_stderr_fails_the_turn_in_pi_s_words() {
    let stderr = "Error: Model \"gpt-6.1-sol\" is ambiguous across providers: azure/gpt-6.1-sol, \
        openai/gpt-6.1-sol. More than one matching provider is authenticated. Use --provider or \
        provider/model.";
    let (_, end) = read_lines([stderr], 1);
    let said = "Pi reported an error: Model \"gpt-6.1-sol\" is ambiguous across providers: \
        azure/gpt-6.1-sol, openai/gpt-6.1-sol. More than one matching provider is authenticated. \
        Use --provider or provider/model.";
    assert_eq!(
        end.outcome,
        TurnOutcome::Failed {
            reason: said.to_owned()
        }
    );
    assert_eq!(
        end.final_reply.map(|reply| reply.text).as_deref(),
        Some(said)
    );
}

/// A line of noise from a process that exited 0 without a result is still no result.
#[test]
fn stray_text_without_a_failed_exit_is_no_result() {
    let (_, end) = read_lines(["warming up"], 0);
    assert_eq!(end.outcome, TurnOutcome::NoResult);
    assert!(end.final_reply.is_none());
}

/// The session header, and only the header, opens the session.
#[test]
fn every_run_opens_its_session_on_the_header_line_only() {
    for case in cases() {
        let (reads, _) = read(case.fixture);
        let opened: Vec<_> = reads
            .iter()
            .enumerate()
            .filter_map(|(at, read)| read.session.as_ref().map(|seen| (at, seen.session.clone())))
            .collect();
        assert_eq!(
            opened,
            [(0, Some(SessionId::new(case.session)))],
            "{}",
            case.fixture
        );
    }
}

/// Retry handling. In `h1-retry.jsonl` the first provider call fails with a retryable 429, Pi ends
/// that run (`agent_end` with `willRetry: true`), retries, and settles once. A reader that ended at
/// the first `agent_end` would report the failed call. Cut at that `agent_end`, the stream has not
/// settled, so the turn has no result.
#[test]
fn the_turn_ends_at_agent_settled_not_at_the_first_agent_end() {
    let text = fixture("h1-retry.jsonl");
    let lines: Vec<&str> = text.lines().collect();
    let first_end = lines
        .iter()
        .position(|line| line.contains(r#""type":"agent_end""#))
        .expect("an agent_end");
    assert!(lines[first_end].contains(r#""willRetry":true"#));

    let (_, whole) = read_lines(lines.iter().copied(), 0);
    assert_eq!(whole.outcome, TurnOutcome::Completed);
    assert_eq!(whole.last_message.as_deref(), Some("stub reply 2"));

    let (_, cut) = read_lines(lines[..=first_end].iter().copied(), 0);
    assert_eq!(cut.outcome, TurnOutcome::NoResult);
    assert_eq!(cut.last_message, None);
    assert_eq!(tokens(&cut), None, "the failed call reported no tokens");
}

/// Cumulative figures. Pi restates a call's cumulative figure on several lines. In `h1-retry.jsonl` the
/// successful call carries 10 input and 3 output tokens on each of three `message_update` lines and
/// again on `message_end`. In `l2-tool.jsonl` each of the two calls carries its figure on `message_end` and again
/// on `turn_end`. Adding each `message_update` would give `h1-retry.jsonl` 40 input tokens, and adding `turn_end`
/// beside `message_end` would give `l2-tool.jsonl` 2440.
#[test]
fn each_call_is_charged_once_whatever_line_restates_it() {
    let restating = |name: &str, line_type: &str, figure: &str| {
        fixture(name)
            .lines()
            .filter(|line| {
                line.contains(&format!(r#""type":"{line_type}""#)) && line.contains(figure)
            })
            .count()
    };
    assert_eq!(
        restating("h1-retry.jsonl", "message_update", r#""input":10,"#),
        3
    );
    assert_eq!(restating("l2-tool.jsonl", "turn_end", r#""input":592,"#), 1);
    assert_eq!(restating("l2-tool.jsonl", "turn_end", r#""input":628,"#), 1);
    assert_eq!(tokens(&read("h1-retry.jsonl").1), Some([10, 3, 0, 0]));
    assert_eq!(tokens(&read("l2-tool.jsonl").1), Some([1220, 25, 0, 0]));
}

/// The live provider leaves every `message_update` at zero and reports a call's figure first on
/// `message_end`, so a reader of `message_update` alone would charge `l1-success.jsonl` nothing.
#[test]
fn a_figure_first_reported_on_message_end_is_charged() {
    let text = fixture("l1-success.jsonl");
    let updates: Vec<&str> = text
        .lines()
        .filter(|line| line.contains(r#""type":"message_update""#))
        .collect();
    assert_eq!(updates.len(), 3);
    assert!(
        updates
            .iter()
            .all(|line| line.contains(r#""input":0,"output":0,"#))
    );
    assert_eq!(tokens(&read("l1-success.jsonl").1), Some([578, 5, 0, 0]));
}

/// Session resume. The second turn in `l3-turn2.jsonl` resumed the first under `--session`. It kept the session, and
/// the model recalled the word the first turn gave it. The reader reports 603 input and 6 output
/// tokens for it, scoped to the turn. A running total for the session could only grow, and 6 is
/// fewer than the first turn's 10, so the figure is the resumed turn's own call.
#[test]
fn a_resumed_turn_reports_only_its_own_calls() {
    let (_, first) = read("l3-turn1.jsonl");
    let (_, resumed) = read("l3-turn2.jsonl");
    assert_eq!(
        first.session, resumed.session,
        "the second turn resumed the first"
    );
    assert_eq!(resumed.last_message.as_deref(), Some("PLUM"));
    assert_eq!(tokens(&first), Some([578, 10, 0, 0]));
    assert_eq!(tokens(&resumed), Some([603, 6, 0, 0]));
    assert!(matches!(resumed.usage, Some(ScopedUsage::Turn(_))));
}

/// Compaction. In `c1-compaction.jsonl` Pi compacted after the agent run and before `agent_settled`, and
/// summarised through a second call to the stub, which reported 22 input and 3 output tokens. No
/// message event carries that call. Its figure arrives only on `compaction_end`, as `result.usage`.
#[test]
fn a_compaction_summary_call_is_charged() {
    let text = fixture("c1-compaction.jsonl");
    let carrying = |figure: &str| {
        text.lines()
            .filter(|line| line.contains(figure))
            .map(|line| {
                line.split(r#""type":""#)
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(carrying(r#""input":22,"#), [Some("compaction_end")]);
    let (_, end) = read("c1-compaction.jsonl");
    assert_eq!(tokens(&end), Some([33, 6, 0, 0]));
    assert_eq!(
        end.model_usage
            .get("stub-1")
            .map(|usage| usage.input.count()),
        Some(11),
        "the summary call names no model, so only the agent's call is attributed"
    );
}

/// Killed mid-call. In `l2-tool.jsonl` the run is killed during its second call: the first call finished at 592 and 18,
/// and the live provider left the second at zero until its `message_end`, which never came. What the
/// second call consumed is unknown, so the turn has no usage rather than the first call's alone.
#[test]
fn a_turn_killed_during_a_call_still_at_zero_has_no_usage() {
    let text = fixture("l2-tool.jsonl");
    let lines: Vec<&str> = text.lines().collect();
    let second_start = lines
        .iter()
        .rposition(|line| line.contains(r#""type":"message_start","message":{"role":"assistant""#))
        .expect("a second call");
    let second_end = second_start
        + lines[second_start..]
            .iter()
            .position(|line| line.contains(r#""type":"message_end""#))
            .expect("its message_end");

    let killed = |upto: usize| {
        let (_, end) = read_lines(lines[..upto].iter().copied(), 0);
        let spent = tokens(&end);
        (end.outcome, spent)
    };
    assert_eq!(killed(second_end), (TurnOutcome::NoResult, None));
    assert_eq!(
        killed(second_end + 1),
        (TurnOutcome::NoResult, Some([1220, 25, 0, 0])),
        "control: cut after the second call ended, both calls are charged"
    );
}

/// Fixture hygiene: the runs were normalised before they were committed (see the README).
#[test]
fn pi_fixtures_carry_no_operator_paths() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi");
    let mut names: Vec<String> = fs::read_dir(&dir)
        .expect("fixture dir")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .map(|path| {
            path.file_name()
                .expect("a file")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    let mut expected: Vec<&str> = cases().iter().map(|case| case.fixture).collect();
    expected.sort_unstable();
    assert_eq!(names, expected, "one case per fixture");
    for name in names {
        for (n, line) in fixture(&name).lines().enumerate() {
            for needle in [
                concat!("/Use", "rs/"),
                "/private/tmp",
                "/home/",
                "/var/folders",
            ] {
                assert!(
                    !line.contains(needle),
                    "{name}:{}: contains `{needle}`",
                    n + 1
                );
            }
        }
    }
}
