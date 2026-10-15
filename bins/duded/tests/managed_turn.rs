//! The whole of a managed turn's end, through the real host: a child writes a `stream-json` turn
//! to its standard output, exits, and a `TurnEnded` carrying `Some(Usage)` reaches the journal.
//!
//! # What this is the only test of
//!
//! Every other piece has a test of its own — `dudes-providers` proves what a stream means,
//! `dudes-host` proves the pipe is drained and the child reaped, and this package's own
//! `children.rs` proves a recycled identifier cannot answer for a turn. None of them can say that
//! the pieces are **joined**, and joined is what this test checks: a parser that nothing in production feeds
//! leaves `Input::ChildExited` with no production construction site, and `usage: None` would be
//! the only path a managed turn had instead of the rare crash path.
//!
//! So the assertion is `Some(Usage)` with its numbers, and the control is a second run of the same
//! daemon over a child that writes **no** `result` line: that one still ends, and ends `NoResult`
//! with no usage. Without the control, a `TurnEnded` that carried a usage invented from nothing
//! would pass.
//!
//! # The vendors are local stand-ins
//!
//! No vendor account is installed on a runner and none is needed. The accounting tests launch this
//! binary through a small adapter and re-execute [`the_helper_entry_point`]. The session test uses
//! the real `ClaudeAdapter` to launch `examples/two-roles/bin/claude`, whose stand-in accepts the
//! composed vendor flags and records whether each turn used `--session-id` or `--resume`.
//!
//! Both paths still build argv from typed parts and exec a program; nothing goes near a shell
//! string (invariant 7).
#![cfg(unix)]

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use duded::settings::Seams;
use duded::startup::Witness;
use duded::{ChildEnds, ChildTurn, ExitCause, boot};
use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_proto::{ExitStatus, Record, TurnOutcome};
use dudes_providers::adapter::{
    Adapter, AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
};
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::SESSION_ACQUISITION;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::claude::stream::TurnStream;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_providers::stream::TurnReader;
use dudes_supervisor::host::AdoptionKey;
use dudes_supervisor::testing::FakeRng;
use dudes_types::{ModelSelection, Provider, Tier};

mod common;

use common::{Fixture, ScriptedRunner, TestClock};

/// Which turn the re-executed binary should play, and where the value is read from.
const ROLE: &str = "DUDES_MANAGED_TURN_ROLE";
/// Announce a session, then a `result` line with a cost: a turn that ended normally.
const COMPLETE: &str = "complete";
/// Announce a session and stop: a stream with no `result` line at all.
const NO_RESULT: &str = "no-result";
/// Announce a session, then a `result` line whose `modelUsage` names **two** models — a main loop
/// and a subagent — with different counts and a cost for the first alone.
///
/// That line is **hand-authored, not captured**: no recorded run under
/// `crates/dudes-providers/tests/fixtures/claude/` has more than one `modelUsage` key, so this
/// proves the map survives every hop of the real end path and never that the vendor emits two keys.
/// The same limitation, and what would settle it, is written out at
/// `dudes-providers`' `src/claude/usage.rs::a_two_key_result_keeps_both_models_and_invents_no_cost`.
const TWO_MODELS: &str = "two-models";
/// The session the vendor announces.
const SESSION: &str = "3f9a1c2e-0000-4000-8000-0000000012ab";

/// The re-executed entry point: be the vendor CLI for one turn, or be an ordinary passing test.
///
/// `writeln!` on the locked handle rather than `println!`, because `clippy::print_stdout` is a
/// workspace lint: this is a test binary *being* a vendor CLI, and the daemon reads what it writes.
#[test]
fn the_helper_entry_point() {
    use std::io::Write as _;

    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(
        out,
        r#"{{"type":"system","subtype":"init","session_id":"{SESSION}","tools":[]}}"#
    );
    if role == TWO_MODELS {
        let _ = writeln!(
            out,
            r#"{{"type":"result","subtype":"success","is_error":false,"session_id":"{SESSION}","result":"both models ran","total_cost_usd":0.9,"modelUsage":{{"claude-haiku-4-5":{{"inputTokens":5,"outputTokens":9,"cacheReadInputTokens":0,"cacheCreationInputTokens":0,"costUSD":0.4}},"claude-opus-4-1-20250805":{{"inputTokens":7,"outputTokens":3,"cacheReadInputTokens":2,"cacheCreationInputTokens":0}}}}}}"#
        );
    }
    if role == COMPLETE {
        let _ = writeln!(
            out,
            r#"{{"type":"result","subtype":"success","is_error":false,"session_id":"{SESSION}","result":"the goal is met","total_cost_usd":0.75,"modelUsage":{{"claude-opus-4":{{"inputTokens":5,"outputTokens":9,"cacheReadInputTokens":0,"cacheCreationInputTokens":0}}}}}}"#
        );
    }
    let _ = out.flush();
    // Not `return`: the harness would print its own summary onto the stream the daemon is reading,
    // and a turn is over when its child exits.
    std::process::exit(0);
}

/// An adapter whose managed launch is this binary in the [`ROLE`] the daemon's environment names.
///
/// Pure, like every adapter: it reads no environment of its own and no clock. The role travels in
/// `Settings.daemon_env`, which is what `AgentCtx.daemon_env` carries, so two calls with equal
/// inputs still give equal bytes.
#[derive(Debug)]
struct HelperAdapter;

impl Adapter for HelperAdapter {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    fn program(&self) -> &std::ffi::OsStr {
        std::ffi::OsStr::new("claude")
    }

    fn tier(&self) -> Tier {
        Tier::HookGoverned
    }

    fn isolation(&self) -> Isolation {
        Isolation::None
    }

    fn session_acquisition(&self) -> SessionAcquisition {
        SESSION_ACQUISITION
    }

    fn turn_reader(&self) -> Box<dyn TurnReader> {
        Box::new(TurnStream::new())
    }

    fn model_selection(&self) -> ModelSelection {
        ModelSelection::provider_default()
    }

    fn managed(
        &self,
        ctx: &AgentCtx<'_>,
        _task: &str,
        _session: &TurnSession,
        _selection: &ModelSelection,
    ) -> Result<LaunchSpec, dudes_providers::launch::LaunchError> {
        Ok(LaunchSpec {
            argv: helper_argv(),
            env: ctx.daemon_env.to_vec(),
            cwd: ctx.cwd.to_path_buf(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        })
    }

    fn managed_install(&self, _ctx: &AgentCtx<'_>, _selection: &ModelSelection) -> ManagedInstall {
        ManagedInstall::default()
    }

    fn usage_source(&self) -> UsageSource {
        UsageSource::HeadlessResult
    }

    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        None
    }

    /// No variable bills a turn on this stand-in (the empty answer).
    fn billing_env_vars(&self) -> &'static [&'static str] {
        &[]
    }
}

/// This binary, re-invoked so that the harness runs exactly [`the_helper_entry_point`]. Typed
/// parts, one element each; nothing is concatenated and nothing reaches a shell.
fn helper_argv() -> Vec<OsString> {
    let mut argv = vec![OsString::from(
        std::env::current_exe().expect("current_exe"),
    )];
    argv.extend(["--exact", "the_helper_entry_point", "--quiet"].map(OsString::from));
    argv
}

/// The environment a helper child needs, plus the role it should play.
fn daemon_env(role: &str) -> Vec<EnvVar> {
    let mut env: Vec<EnvVar> = ["PATH", "HOME", "TMPDIR"]
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| EnvVar::new(*name, value)))
        .collect();
    env.push(EnvVar::new(ROLE, role));
    env
}

/// The environment the example stand-in reads, through the real adapter's scrub.
fn standin_env(work: &Path) -> Vec<EnvVar> {
    let mut env: Vec<EnvVar> = ["PATH", "HOME", "TMPDIR"]
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| EnvVar::new(*name, value)))
        .collect();
    env.extend([
        EnvVar::new("DUDES_EXAMPLE_WORK", work.as_os_str()),
        EnvVar::new("DUDES_EXAMPLE_MARK_ON_RUN", "2"),
        EnvVar::new("DUDES_EXAMPLE_HOLD_SECONDS", "1"),
    ]);
    env
}

/// `duded::ChildEnds` as the host crate's stream sink — `main.rs`'s bridge, which a test cannot
/// import from a binary target.
struct HostStreams(Arc<ChildEnds>);

impl ChildStreams for HostStreams {
    fn watch(&self, key: Option<&AdoptionKey>) -> Box<dyn ChildStream> {
        Box::new(HostChildTurn(self.0.watch(key)))
    }
}

struct HostChildTurn(ChildTurn);

impl ChildStream for HostChildTurn {
    fn line(&mut self, text: &str) {
        self.0.line(text);
    }

    fn ended(self: Box<Self>, exit: ChildExit) {
        self.0.ended_with_elapsed(
            ExitStatus {
                code: exit.code,
                signal: exit.signal,
            },
            exit.elapsed_ms,
        );
    }
}

/// Run one managed turn whose child plays `role`, and return the `TurnEnded` it produced.
fn one_managed_turn(role: &str) -> Record {
    let fixture = Fixture::new();
    let settings = duded::settings::Settings {
        daemon_env: daemon_env(role),
        ..fixture.settings()
    };

    // The sink first, then the host that writes into it — the same order `main.rs` uses, and the
    // reason `Seams` carries it: the host is built before the loop exists.
    let children = ChildEnds::new();
    let seams = Seams {
        host: RealHost::watching(Arc::new(HostStreams(Arc::clone(&children)))),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        // One passing predicate, so the goal is met after one turn and the loop issues no second
        // one while the assertions are being read.
        predicates: Arc::new(ScriptedRunner::scripted([common::passes()])),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
        resolver: duded::classify::Resolver::fixed(std::sync::Arc::new(duded::classify::NoWalk)),
        // Collected rather than built from a table: this test's provider is a helper, not a
        // configured one, and the registry keys it on what it says it serves.
        adapters: [Box::new(HelperAdapter) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children,
        witness: Witness::default(),
    };
    let daemon = boot(&settings, seams).expect("boot");
    let mut running = common::start(daemon);

    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );

    let ended = fixture.await_record("TurnEnded", |record| match record {
        Record::TurnEnded { .. } => Some(record.clone()),
        _ => None,
    });
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    ended
}

/// Assert the durable session sequence and the stand-in's independent launch transcript.
fn assert_two_turn_session(records: &[Record], work: &Path) {
    let issued: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::TurnIssued { turn, session, .. } => Some((*turn, session.as_ref())),
            _ => None,
        })
        .collect();
    assert_eq!(
        issued.len(),
        2,
        "the failed predicate causes exactly two turns"
    );
    let first_session = issued[0].1.expect("the first turn announces a session");
    assert!(!first_session.as_str().is_empty());
    assert_eq!(
        issued[1].1,
        Some(first_session),
        "the second turn resumes the vendor-confirmed id"
    );
    let learned: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::SessionLearned { turn, session, .. } => Some((*turn, session)),
            _ => None,
        })
        .collect();
    assert_eq!(
        learned,
        vec![(issued[0].0, first_session), (issued[1].0, first_session)],
        "each turn durably confirms the session it announced"
    );

    let transcript = work
        .join("transcripts")
        .join(format!("{}.jsonl", first_session.as_str()));
    let launches: Vec<_> = std::fs::read_to_string(&transcript)
        .unwrap_or_else(|error| panic!("reading {}: {error}", transcript.display()))
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("stand-in transcript"))
        .filter(|line| line["type"] == "init")
        .map(|line| {
            line["launched_with"]
                .as_str()
                .expect("an init names its session flag")
                .to_owned()
        })
        .collect();
    assert_eq!(launches, ["--session-id", "--resume"]);
    assert_eq!(
        std::fs::read_dir(work.join("transcripts"))
            .expect("the transcript directory")
            .count(),
        1,
        "both turns wrote the one confirmed session"
    );
}

/// The real Claude launch learns the first turn's vendor-confirmed session before the
/// next turn is issued, so the second launch resumes it. The example stand-in supplies only the
/// vendor wire and transcript; the adapter, process host, child join, machine and journal are the
/// shipped path.
#[test]
fn two_managed_turns_resume_the_session_the_vendor_confirmed() {
    let fixture = Fixture::new();
    let work = fixture.state.path().join("stand-in");
    std::fs::create_dir(&work).expect("the stand-in work directory");
    for run in [1, 2] {
        std::fs::write(work.join(format!("release.{run}")), b"")
            .expect("the stand-in turn is released");
    }
    let command = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/two-roles/bin/claude")
        .canonicalize()
        .expect("the example stand-in");
    let settings = duded::settings::Settings {
        daemon_env: standin_env(&work),
        ..fixture.settings()
    };
    let children = ChildEnds::new();
    let seams = Seams {
        host: RealHost::watching(Arc::new(HostStreams(Arc::clone(&children)))),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(ScriptedRunner::scripted([
            common::fails(),
            common::passes(),
        ])),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
        resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
        adapters: [Box::new(ClaudeAdapter::new(ClaudeConfig {
            command: command.into_os_string(),
            ..ClaudeConfig::default()
        })) as Box<dyn Adapter>]
        .into_iter()
        .collect(),
        children,
        witness: Witness::default(),
    };
    let daemon = boot(&settings, seams).expect("boot");
    let mut running = common::start(daemon);

    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    fixture.await_record("a Done decision after two turns", |record| {
        matches!(
            record,
            Record::LoopDecided {
                decision: dudes_proto::LoopDecision::Done,
                ..
            }
        )
        .then_some(())
    });
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    assert_two_turn_session(&fixture.records(), &work);
}

/// A managed turn ending normally produces `TurnEnded` carrying `Some(Usage)`.
///
/// Every number is asserted, not just that the option is `Some`: a `Usage::ZERO` reached by a sum
/// over no models would satisfy `is_some()` while accounting nothing, and invariant 4 admits
/// against the value rather than against its presence. The outcome and the last message go with
/// it, because those come from the same `result` line and a turn that ended `Completed` with no
/// message would mean the line was read only halfway.
#[test]
fn a_managed_turn_ends_with_the_usage_its_stream_reported() {
    let Record::TurnEnded {
        usage,
        outcome,
        last_message,
        exit,
        provenance,
        ..
    } = one_managed_turn(COMPLETE)
    else {
        unreachable!("await_record matched on the variant")
    };

    let usage = usage.expect("a managed turn that reported a cost ends with Some(Usage)");
    assert_eq!(usage.usd, Some(dudes_types::Usd::from_micros(750_000)));
    assert_eq!(usage.input.count(), 5);
    assert_eq!(usage.output.count(), 9);
    assert_eq!(outcome, Some(TurnOutcome::Completed));
    assert_eq!(last_message.as_deref(), Some("the goal is met"));
    assert_eq!(
        exit,
        ExitStatus {
            code: Some(0),
            signal: None
        },
        "the exit status is the one the platform reported, so the end is the child's own"
    );
    assert_eq!(
        provenance,
        dudes_types::Provenance::Observed,
        "invariant 3: a turn's end is what the daemon saw"
    );
}

/// A `result` line naming two models carries **both** keys, through the real
/// `ChildExited` path, into the `TurnEnded` the journal receives.
///
/// # Why this runs end to end rather than as a unit
///
/// The map crosses four hops — `stream.rs` reads it off the result line, `TurnEnd` carries it,
/// `children.rs` posts it on `Input::ChildExited`, and `machine.rs` writes it onto `TurnEnded`.
/// A unit test at any one of them is green while any of the other three drops it. This is the only
/// test that fails if the map is lost at *any* hop.
///
/// # The three things asserted, and what each excludes
///
/// 1. **Two keys, spelled as the vendor spelled them.** Excludes a collapse to one "effective
///    model" string, and excludes normalisation — the two keys here are a bare alias and a dated
///    id, deliberately, because retaining them raw is what keeps the normalisation question
///    askable later.
/// 2. **Each key's own counts.** Excludes a map built by copying the aggregate onto every key.
/// 3. **The unpriced key stays unpriced while the aggregate takes the top-level total.** Excludes
///    the one implementation that satisfies 1 and 2 and still breaks invariant 4: apportioning
///    `total_cost_usd` across the keys.
///
/// The control is `a_stream_with_no_result_still_ends_and_carries_no_usage` above, which reaches
/// this same code path with no result line at all: it must carry an **empty** map, so a
/// `model_usage` invented anywhere but the stream would show up there.
#[test]
fn a_result_naming_two_models_carries_both_into_the_journal() {
    let Record::TurnEnded {
        usage, model_usage, ..
    } = one_managed_turn(TWO_MODELS)
    else {
        unreachable!("await_record matched on the variant")
    };

    assert_eq!(
        model_usage.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["claude-haiku-4-5", "claude-opus-4-1-20250805"],
        "both raw keys survive every hop from the result line to the record"
    );
    assert_eq!(model_usage["claude-haiku-4-5"].output.count(), 9);
    assert_eq!(model_usage["claude-opus-4-1-20250805"].output.count(), 3);
    assert_eq!(
        model_usage["claude-opus-4-1-20250805"].cache_read.count(),
        2
    );
    assert_eq!(
        model_usage["claude-haiku-4-5"].usd,
        Some(dudes_types::Usd::from_micros(400_000)),
        "a reported per-model cost is that key's own"
    );
    assert_eq!(
        model_usage["claude-opus-4-1-20250805"].usd, None,
        "and a key the vendor priced nothing for takes no share of the total"
    );

    let usage = usage.expect("the top-level total is charged as it always was");
    assert_eq!(
        usage.usd,
        Some(dudes_types::Usd::from_micros(900_000)),
        "the aggregate is still the top-level total, not a sum of the breakdown"
    );
    assert_eq!(usage.output.count(), 12, "9 + 3, the saturating sum");
}

/// The other half of the control: a turn whose stream carried no `result` line records an **empty**
/// breakdown, not an invented one.
#[test]
fn a_stream_with_no_result_carries_an_empty_breakdown() {
    let Record::TurnEnded { model_usage, .. } = one_managed_turn(NO_RESULT) else {
        unreachable!("await_record matched on the variant")
    };
    assert!(
        model_usage.is_empty(),
        "no result line means no observation, and an empty map is that claim: {model_usage:?}"
    );
}

/// The control: the same daemon, the same host, the same child — one that announces its session
/// and then exits without a `result` line.
///
/// It still ends, because the exit status alone is enough to close a turn, and it ends `NoResult`
/// with **no** usage. That is what makes the assertion above a measurement: a `TurnEnded` whose
/// usage came from anywhere but the stream would carry one here too.
#[test]
fn a_stream_with_no_result_still_ends_and_carries_no_usage() {
    let Record::TurnEnded {
        usage,
        outcome,
        last_message,
        ..
    } = one_managed_turn(NO_RESULT)
    else {
        unreachable!("await_record matched on the variant")
    };

    assert_eq!(usage, None);
    assert_eq!(outcome, Some(TurnOutcome::NoResult));
    assert_eq!(last_message, None);
}
