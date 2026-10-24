//! A vendor that reports a session's running total has each turn charged only what that turn
//! added to the running total the journal last recorded for the session.

use std::fs;
use std::path::Path;

use dudes_proto::goal::Evidence;
use dudes_proto::{ExitStatus, Record};
use dudes_providers::stream::{ScopedUsage, SessionObservation, TurnReader};
use dudes_providers::{claude, codex};
use dudes_supervisor::Input;
use dudes_supervisor::testing::Harness;
use dudes_types::{AgentId, GoalId, SessionId, Tokens, TurnId, Usage, Usd};

const EXIT_0: ExitStatus = ExitStatus {
    code: Some(0),
    signal: None,
};

/// A captured run from `dudes-providers`' golden set.
fn fixture(provider: &str, name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../dudes-providers/tests/fixtures")
        .join(provider)
        .join(format!("{name}.stdout"));
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{name}: {error}"))
}

/// A run through its provider's reader: what its stream opened, and the usage it reported.
fn read_run(
    mut reader: Box<dyn TurnReader>,
    text: &str,
) -> (SessionObservation, Option<ScopedUsage>) {
    let opened: Vec<SessionObservation> = text
        .lines()
        .filter_map(|line| reader.line(line).session)
        .collect();
    let [opened] = opened.as_slice() else {
        panic!("the run opens its session once: {opened:?}");
    };
    (opened.clone(), reader.end(EXIT_0).usage)
}

fn codex_run(name: &str) -> (SessionObservation, Option<ScopedUsage>) {
    read_run(
        Box::new(codex::stream::TurnStream::new()),
        &fixture("codex", name),
    )
}

fn tokens(input: u64, cache_read: u64, output: u64) -> Usage {
    Usage {
        input: Tokens::new(input),
        output: Tokens::new(output),
        cache_read: Tokens::new(cache_read),
        ..Usage::ZERO
    }
}

fn session_total(input: u64) -> ScopedUsage {
    ScopedUsage::Session {
        total: tokens(input, 0, 0),
        calls: Usage::ZERO,
    }
}

struct Floor {
    harness: Harness,
    agent: AgentId,
    goal: GoalId,
}

impl Floor {
    fn hire() -> Self {
        let mut harness = Harness::new();
        let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
        Self {
            harness,
            agent,
            goal,
        }
    }

    fn open_turn(&self) -> TurnId {
        self.harness
            .open_turn(self.agent)
            .expect("the loop issued a turn")
    }

    /// End the open turn on `session` with `usage`, then fail the predicate so the loop issues the
    /// next turn.
    fn turn(&mut self, session: &SessionId, usage: Option<ScopedUsage>) {
        let turn = self.open_turn();
        self.harness.drive(self.harness.session_observation(
            turn,
            SessionObservation {
                session: Some(session.clone()),
                ..SessionObservation::default()
            },
        ));
        self.harness.drive(Input::ChildExited {
            report: dudes_proto::TurnReport::default(),
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: EXIT_0,
            usage,
            model_usage: std::collections::BTreeMap::new(),
            outcome: dudes_proto::TurnOutcome::Completed,
            last_message: None,
        });
        self.harness.drive(Input::PredicateDone {
            goal: self.goal,
            pass: false,
            evidence: Evidence::Command {
                argv: vec!["npm".into(), "test".into()],
                program: Some("/opt/toolchain/bin/npm".into()),
                exit: Some(1),
                output_tail: String::new(),
            },
        });
    }

    fn charges(&self) -> Vec<Option<Usage>> {
        self.harness
            .records()
            .filter_map(|record| match record {
                Record::TurnEnded { usage, .. } => Some(*usage),
                _ => None,
            })
            .collect()
    }

    fn running_totals(&self) -> Vec<Option<Usage>> {
        self.harness
            .records()
            .filter_map(|record| match record {
                Record::TurnEnded { session_usage, .. } => Some(*session_usage),
                _ => None,
            })
            .collect()
    }
}

fn thread() -> SessionId {
    SessionId::new("01a0e1d9-3860-7800-a9ff-2994ef3b1c6f")
}

/// The resumed run `c3-resume` reports 32323 input tokens, 28288 of them cached, and 10 output,
/// which is the thread's running total. The vendor's own record of that turn is 16170 input,
/// 16000 of them cached, and 5 output, so it is charged 170 uncached, 16000 cached and 5 output.
/// The record keeps the running total beside the charge, and a replay rebuilds it.
#[test]
fn a_resumed_codex_turn_is_charged_only_its_own_usage() {
    let (original, original_usage) = codex_run("c1-success");
    let (resumed, resumed_usage) = codex_run("c3-resume");
    let thread = original.session.clone().expect("c1 names its thread");
    assert_eq!(resumed.session.as_ref(), Some(&thread));

    let mut floor = Floor::hire();
    floor.turn(&thread, original_usage);
    floor.turn(&thread, resumed_usage);
    assert_eq!(
        floor.charges(),
        [Some(tokens(3865, 12_288, 5)), Some(tokens(170, 16_000, 5))]
    );
    assert_eq!(
        floor.running_totals(),
        [
            Some(tokens(3865, 12_288, 5)),
            Some(tokens(4035, 28_288, 10))
        ]
    );

    floor.harness.restart();
    assert_eq!(
        floor.harness.supervisor.world().session_total(&thread),
        tokens(4035, 28_288, 10),
        "the replayed session total is the vendor's running total"
    );
}

/// A turn that recorded no usage, such as one killed before its last line, is paid for by the
/// next turn on the session: its tokens are inside the next running total.
#[test]
fn the_next_turn_pays_for_a_turn_that_recorded_no_usage() {
    let thread = SessionId::new("01a0e1d9-3860-7800-a9ff-2994ef3b1c6f");
    let mut floor = Floor::hire();
    floor.turn(&thread, Some(session_total(100)));
    floor.turn(&thread, None);
    floor.turn(&thread, Some(session_total(250)));
    assert_eq!(
        floor.charges(),
        [Some(tokens(100, 0, 0)), None, Some(tokens(150, 0, 0))]
    );
}

/// A running total below the session's last recorded one means the vendor started counting
/// again, so that turn is charged the whole figure, never zero.
#[test]
fn a_falling_running_total_is_charged_whole() {
    let thread = SessionId::new("01a0e1d9-3860-7800-a9ff-2994ef3b1c6f");
    let mut floor = Floor::hire();
    floor.turn(&thread, Some(session_total(100)));
    floor.turn(&thread, Some(session_total(40)));
    assert_eq!(
        floor.charges(),
        [Some(tokens(100, 0, 0)), Some(tokens(40, 0, 0))]
    );
}

/// After a reset the next turn is charged from the new segment: 90 is charged 50, what the
/// vendor counted since the reset turn's 40.
#[test]
fn after_a_reset_a_later_turn_is_charged_from_the_new_segment() {
    let mut floor = Floor::hire();
    for total in [100, 40, 90] {
        floor.turn(&thread(), Some(session_total(total)));
    }
    assert_eq!(
        floor.charges(),
        [
            Some(tokens(100, 0, 0)),
            Some(tokens(40, 0, 0)),
            Some(tokens(50, 0, 0))
        ]
    );
}

/// A total that climbs well past the pre-reset one is still charged only what it added: 300 is
/// charged 210, and the session is short of nothing.
#[test]
fn a_running_total_that_climbs_again_is_charged_what_it_added() {
    let mut floor = Floor::hire();
    for total in [100, 40, 90, 300] {
        floor.turn(&thread(), Some(session_total(total)));
    }
    assert_eq!(
        floor.charges(),
        [
            Some(tokens(100, 0, 0)),
            Some(tokens(40, 0, 0)),
            Some(tokens(50, 0, 0)),
            Some(tokens(210, 0, 0))
        ]
    );
}

/// A restart between the reset and the next turn charges the same: the baseline comes from the
/// journal, not from the run that wrote it.
#[test]
fn a_restart_after_a_reset_charges_the_same() {
    let mut floor = Floor::hire();
    floor.turn(&thread(), Some(session_total(100)));
    floor.turn(&thread(), Some(session_total(40)));
    floor.harness.restart();
    floor.turn(&thread(), Some(session_total(90)));
    let charges = floor.charges();
    assert_eq!(charges.last(), Some(&Some(tokens(50, 0, 0))), "{charges:?}");
}

/// A journal written before the running total was recorded replays by the sum of its charges:
/// 100 and 150 were charged 100 and 50, so a later total of 200 is charged 50.
#[test]
fn a_journal_without_running_totals_replays_by_the_sum_of_charges() {
    let mut floor = Floor::hire();
    floor.turn(&thread(), Some(session_total(100)));
    floor.turn(&thread(), Some(session_total(150)));
    for entry in &mut floor.harness.journal {
        if let Record::TurnEnded { session_usage, .. } = &mut entry.record {
            *session_usage = None;
        }
    }
    floor.harness.restart();
    floor.turn(&thread(), Some(session_total(200)));
    let charges = floor.charges();
    assert_eq!(charges.last(), Some(&Some(tokens(50, 0, 0))), "{charges:?}");
}

/// A turn that resumed one thread while the vendor answered with another has its total charged
/// whole, and its total becomes no part of the resumed thread's baseline.
#[test]
fn a_refused_resume_is_charged_its_whole_running_total() {
    let resumed = SessionId::new("01a0e1d9-3860-7800-a9ff-2994ef3b1c6f");
    let answered = SessionId::new("01a0e1d9-84d5-7bc2-8567-d32735103d71");
    let mut floor = Floor::hire();
    floor.turn(&resumed, Some(session_total(100)));
    floor.turn(&answered, Some(session_total(250)));
    assert_eq!(
        floor.charges(),
        [Some(tokens(100, 0, 0)), Some(tokens(250, 0, 0))]
    );
    assert_eq!(
        floor.harness.supervisor.world().session_total(&resumed),
        tokens(100, 0, 0)
    );
}

/// The captured Claude session, each turn through the Claude reader, with each result line passed
/// through `edit` first.
fn claude_session(edit: impl Fn(&str, &str) -> String) -> Floor {
    let mut floor = Floor::hire();
    for name in ["s1-first", "s2-resume", "s3-resume"] {
        let text = edit(name, &fixture("claude", name));
        let (opened, usage) = read_run(Box::new(claude::stream::TurnStream::new()), &text);
        assert_eq!(
            opened.session.as_ref().map(SessionId::as_str),
            Some("b1d8abd9-af97-4629-98f9-f4bd724f8b95"),
            "{name} runs on the captured session"
        );
        floor.turn(opened.session.as_ref().unwrap(), usage);
    }
    floor
}

fn claude_usage([input, output, cache_read, cache_creation]: [u64; 4], micros: u64) -> Usage {
    Usage {
        input: Tokens::new(input),
        output: Tokens::new(output),
        cache_read: Tokens::new(cache_read),
        cache_creation: Tokens::new(cache_creation),
        usd: Some(Usd::from_micros(micros)),
    }
}

/// Each turn of the captured session made one call of its own: 2 input and 4 output tokens, 11095
/// read from the cache, 3867, 3930 and 3993 written, costing $0.033243, $0.033747 and $0.034251.
fn claude_turns() -> [Option<Usage>; 3] {
    [
        Some(claude_usage([2, 4, 11_095, 3867], 33_243)),
        Some(claude_usage([2, 4, 11_095, 3930], 33_747)),
        Some(claude_usage([2, 4, 11_095, 3993], 34_251)),
    ]
}

/// Claude Code 2.1.283 reports a resumed turn's `modelUsage` and `total_cost_usd` as the session's
/// running total: the captured turns report output 4, 8 and 12. Each turn is charged only its own
/// call, and the record keeps the running total beside the charge.
#[test]
fn a_resumed_claude_turn_is_charged_only_its_own_usage() {
    let floor = claude_session(|_, text| text.to_owned());
    assert_eq!(floor.charges(), claude_turns());
    assert_eq!(
        floor.running_totals(),
        [
            Some(claude_usage([2, 4, 11_095, 3867], 33_243)),
            Some(claude_usage([4, 8, 22_190, 7797], 66_990)),
            Some(claude_usage([6, 12, 33_285, 11_790], 101_241)),
        ]
    );
}

/// Derived from the capture, not captured: the same session as a release that reports each turn's
/// own figure would print it, with each result line's `modelUsage` and `total_cost_usd` set to the
/// turn's own call. Such a figure adds only 63 cache-write tokens to the recorded one, which is
/// below the turn's own call, so each turn is charged its figure whole and never that difference.
#[test]
fn a_claude_turn_that_reports_its_own_usage_is_charged_it_whole() {
    let floor = claude_session(|name, text| {
        let cost = match name {
            "s1-first" => 0.033_243,
            "s2-resume" => 0.033_747,
            _ => 0.034_251,
        };
        text.lines()
            .map(|line| {
                let mut line: serde_json::Value = serde_json::from_str(line).unwrap();
                if line["type"] == "result" {
                    let call = line["usage"].clone();
                    let model = &mut line["modelUsage"]["claude-opus-5-5"];
                    model["inputTokens"] = call["input_tokens"].clone();
                    model["outputTokens"] = call["output_tokens"].clone();
                    model["cacheReadInputTokens"] = call["cache_read_input_tokens"].clone();
                    model["cacheCreationInputTokens"] = call["cache_creation_input_tokens"].clone();
                    model["costUSD"] = cost.into();
                    line["total_cost_usd"] = cost.into();
                }
                line.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    assert_eq!(floor.charges(), claude_turns());
    assert_eq!(
        floor.running_totals(),
        claude_turns(),
        "the edited figures are the turns' own"
    );
}
