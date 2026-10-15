//! The two managed-run limits come from `agent.hire`, survive a crash, and never reach
//! the journal as a value the record cannot mean.
//!
//! # Why both tests drive the socket rather than `Handle::ask`
//!
//! The zero refusal lives at the JSON-RPC door (`dudes-rpc/src/methods/agent.rs`), beside the
//! `sandbox` one and for the same reason: it is the single door both the CLI and a direct client
//! come through, and a refusal in front of `Control::hire` journals nothing. `Handle::ask` is the
//! *port behind* that door — most driving tests in this directory use it because they are not about
//! the door — so a test of the refusal that called `ask` would skip the thing under test and pass
//! against an implementation that had no refusal at all.
//!
//! The typed refusals below need the socket for a second reason: `-1`, `1.5`, `"3"` and `2^32` are
//! not values `HireParams` can hold, so they can only be sent as JSON. A test that built a
//! `HireParams` in Rust could not express the mistake it is asserting about.
//!
//! # Why "before the journal" is asserted on the disk
//!
//! `Record::AgentHired` cannot be un-journaled (invariant 1), so *refused* has to mean refused
//! before the append rather than answered with an error afterwards. Only the journal tells those
//! two apart, and `Fixture::records` reads the files the daemon is writing.
#![cfg(unix)]

use std::ffi::{OsStr, OsString};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::Record;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::{ErrorCode, Method};
use dudes_providers::adapter::{Adapter, ManagedInstall};
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::launch::LaunchSpec;
use dudes_providers::registry::Adapters;
use dudes_rpc::Client;
use dudes_supervisor::host::{
    AdoptionKey, HostBoot, KillOutcome, Liveness, ProcessHost, SpawnResult,
};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

/// A host that records every launch it is **asked** for, and leaves the first one unconfirmed.
///
/// `UnconfirmedHost` in `common` refuses before it reaches the fake underneath, so the spec of the
/// refused launch is never recorded — and the refused launch is exactly the one this file needs to
/// see, because it is the *first* launch whose argv the re-issue has to reproduce. This host logs
/// the spec first and decides afterwards.
#[derive(Clone)]
struct LoggingHost {
    inner: SharedHost,
    seen: Arc<Mutex<Vec<LaunchSpec>>>,
    unconfirm_next: Arc<AtomicBool>,
}

impl LoggingHost {
    fn new(unconfirm_first: bool) -> Self {
        Self {
            inner: SharedHost::new(),
            seen: Arc::new(Mutex::new(Vec::new())),
            unconfirm_next: Arc::new(AtomicBool::new(unconfirm_first)),
        }
    }

    /// The argv of each launch this host was asked for, in order.
    fn argv(&self) -> Vec<Vec<OsString>> {
        self.seen
            .lock()
            .expect("no test panics while holding the log")
            .iter()
            .map(|spec| spec.argv.clone())
            .collect()
    }
}

impl ProcessHost for LoggingHost {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        self.seen
            .lock()
            .expect("no test panics while holding the log")
            .push(spec.clone());
        if self.unconfirm_next.swap(false, Ordering::SeqCst) {
            return SpawnResult::CleanupUnconfirmed {
                reason: io::Error::other("cleanup could not be confirmed for this launch"),
            };
        }
        self.inner.spawn(spec, install)
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.inner.kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.inner.alive(key)
    }

    fn boot_identity(&self) -> HostBoot {
        self.inner.boot_identity()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown()
    }
}

/// The credential this daemon minted, read the way an operator's CLI would.
fn credential_of(settings: &duded::Settings) -> OperatorCredential {
    let text = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("the daemon minted a credential");
    OperatorCredential::new(text).expect("it is well formed")
}

type Socket = Client<io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream>;

/// `agent.hire` over the socket with `limits` merged into the well-formed params, as raw JSON.
///
/// Raw rather than a typed `HireParams` because half the rows this file sends are values the type
/// cannot hold. The two share one path so the admitted and the refused rows differ in nothing but
/// the two keys.
fn hire_with(
    client: &mut Socket,
    settings: &duded::Settings,
    fixture: &Fixture,
    limits: &serde_json::Value,
) -> dudes_proto::rpc::Response {
    let mut params =
        serde_json::to_value(common::hire_params(fixture.state.path())).expect("params");
    let object = params.as_object_mut().expect("HireParams is an object");
    for (key, value) in limits.as_object().expect("an object of limit keys") {
        object.insert(key.clone(), value.clone());
    }
    client
        .call_as_operator(Method::AgentHire, Some(params), credential_of(settings))
        .expect("agent.hire is answered")
}

fn hire_result(response: dudes_proto::rpc::Response) -> dudes_proto::rpc::agent::HireResult {
    serde_json::from_value(response.result.expect("a result")).expect("a hire result")
}

/// The `AgentHired` records on the disk, as the pair of recorded limits.
fn recorded_limits(fixture: &Fixture) -> Vec<(Option<u32>, Option<dudes_types::Usd>)> {
    fixture
        .records()
        .iter()
        .filter_map(|record| match record {
            Record::AgentHired {
                max_turns,
                max_budget,
                ..
            } => Some((*max_turns, *max_budget)),
            _ => None,
        })
        .collect()
}

/// Whether `argv` carries `flag` followed by `value`, as the two adjacent parts the composer emits.
fn carries(argv: &[OsString], flag: &str, value: &str) -> bool {
    argv.windows(2)
        .any(|pair| pair[0] == *OsStr::new(flag) && pair[1] == *OsStr::new(value))
}

/// A hire's `max_turns` and `max_budget` reach the composed argv on the first
/// launch and again on the crash re-issue, and a configuration change between the two runs cannot
/// move either value. A hire that asked for neither composes neither flag.
///
/// # The shape, and what each run is for
///
/// **Run one** hires two agents over the socket — one with both limits, one with neither — and
/// gives each a goal. `LoggingHost` leaves the *first* launch unconfirmed, so the capped agent's
/// turn is durable as `TurnIssued` with no `TurnSpawned`: the crash window invariant 2 is about.
/// The control agent's launch goes through. Both argvs are in the log either way, because the host
/// records what it was asked for rather than what it completed.
///
/// **Run two** boots over the same state directory with a **different** `[providers.claude]`
/// table, and replay re-issues the un-spawned turn. The differing table is the arm that makes this
/// a test about *history* rather than about repetition: an implementation that read current
/// configuration instead of the replayed `AgentRow` would still be reading a table, and would still
/// produce two flags, so the same-value assertion would pass. `--max-thinking-tokens 100` appearing
/// in the second argv and not the first is the positive control that the table really did change.
///
/// The control agent is the other half. Without it, a composer that emitted `--max-turns 7` for
/// every launch it ever made would satisfy every assertion above.
#[test]
fn explicit_limits_reach_first_and_reissued_launch() {
    let fixture = Fixture::new();
    let settings = fixture.settings();

    let first_host = LoggingHost::new(true);
    let fixture_parts = seams(
        &settings,
        first_host.clone(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    let daemon = boot(&settings, fixture_parts).expect("boot");
    let mut running = common::start(daemon);
    let mut client = Client::connect(settings.socket()).expect("connect to the daemon");

    let capped = hire_with(
        &mut client,
        &settings,
        &fixture,
        &serde_json::json!({"max_turns": 7, "max_budget": 2_500_000}),
    );
    assert!(
        capped.error.is_none(),
        "positive limits are admitted: {:?}",
        capped.error
    );
    let capped = hire_result(capped);
    common::set_goal(
        &running.handle,
        common::goal_params(capped.agent, &["true"]),
    );

    let plain = hire_with(&mut client, &settings, &fixture, &serde_json::json!({}));
    let plain = hire_result(plain);
    common::set_goal(&running.handle, common::goal_params(plain.agent, &["true"]));
    fixture.await_record("TurnSpawned", |record| {
        matches!(record, Record::TurnSpawned { .. }).then_some(())
    });
    let capped_turn = fixture.await_record("capped TurnIssued", |record| match record {
        Record::TurnIssued { agent, turn, .. } if *agent == capped.agent => Some(*turn),
        _ => None,
    });
    running.stop(ExitCause::Ended);

    let first_run = first_host.argv();
    assert_eq!(
        first_run.len(),
        2,
        "one launch per agent was asked for: {first_run:?}"
    );
    let (capped_argv, plain_argv) = (&first_run[0], &first_run[1]);
    assert!(
        carries(capped_argv, "--max-turns", "7")
            && carries(capped_argv, "--max-budget-usd", "2.500000"),
        "the first launch composes both recorded limits: {capped_argv:?}"
    );
    assert!(
        !plain_argv.iter().any(|part| part == "--max-turns")
            && !plain_argv.iter().any(|part| part == "--max-budget-usd"),
        "an agent hired with no limits gets no flag, not a default: {plain_argv:?}"
    );

    // Run two: the same journal, a different `[providers.claude]` table.
    let second_host = LoggingHost::new(false);
    let mut seams = seams(
        &settings,
        second_host.clone(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::ticking(1_787_394_700_000, 1)),
        Witness::default(),
    );
    let claude = ClaudeConfig {
        extra_args: ["--max-thinking-tokens", "100"]
            .map(OsString::from)
            .to_vec(),
        ..ClaudeConfig::default()
    };
    claude.check(&[]).expect("an honourable table");
    seams.adapters = Adapters::new([Box::new(ClaudeAdapter::new(claude)) as Box<dyn Adapter>]);
    let daemon = boot(&settings, seams).expect("the same state directory boots again");
    let mut running = common::start(daemon);
    fixture.await_record("reissued capped TurnSpawned", |record| {
        (matches!(record, Record::TurnSpawned { agent, turn, reissued: true, .. }
            if *agent == capped.agent && *turn == capped_turn)
            && !second_host.inner.spawned().is_empty())
        .then_some(())
    });
    running.stop(ExitCause::Ended);

    let reissued = second_host.argv();
    assert_eq!(
        reissued.len(),
        1,
        "replay re-issued the un-spawned turn once, and once only: {reissued:?}"
    );
    let reissued = &reissued[0];
    assert!(
        reissued.iter().any(|part| part == "--max-thinking-tokens"),
        "control: the second run really is composing from a different table: {reissued:?}"
    );
    assert!(
        carries(reissued, "--max-turns", "7") && carries(reissued, "--max-budget-usd", "2.500000"),
        "the re-issue composes the same two values, from the replayed record rather than from the \
         table that changed under it: {reissued:?}"
    );
}

/// Every `max_turns` / `max_budget` value the daemon refuses, paired with what it is.
///
/// A function rather than an inline table so the test that consumes it stays inside the workspace's
/// length lint; it is exhaustive over the five kinds of invalid value: zero, negative, fractional,
/// nonnumeric, out-of-range — and each kind appears once per field, since the two fields have
/// different widths and a check that covered only one would miss the other's edge.
fn refused_rows() -> [(&'static str, serde_json::Value); 10] {
    [
        ("a zero turn cap", serde_json::json!({"max_turns": 0})),
        ("a zero budget", serde_json::json!({"max_budget": 0})),
        ("a negative turn cap", serde_json::json!({"max_turns": -1})),
        ("a negative budget", serde_json::json!({"max_budget": -1})),
        (
            "a fractional turn cap",
            serde_json::json!({"max_turns": 1.5}),
        ),
        (
            "a fractional budget",
            serde_json::json!({"max_budget": 1.5}),
        ),
        (
            "a nonnumeric turn cap",
            serde_json::json!({"max_turns": "3"}),
        ),
        (
            "a nonnumeric budget",
            serde_json::json!({"max_budget": "3"}),
        ),
        (
            "an out-of-range turn cap",
            serde_json::json!({"max_turns": 4_294_967_296_u64}),
        ),
        (
            "an out-of-range budget",
            serde_json::json!({"max_budget": 18_446_744_073_709_551_616_f64}),
        ),
    ]
}

/// Absent, `null`, zero and typed-invalid are four outcomes, not one, and the last
/// two never reach the journal.
///
/// # The rows, and what each one would let through if it were missing
///
/// * **Absent** and **`null`** admit as `None`. Without them, a daemon that refused every hire
///   carrying either key would pass every refusal row below.
/// * **Positive one-unit** values admit and are recorded as themselves. Without them, a daemon
///   that refused every *present* value would also pass.
/// * **Zero** is `-32602`, not a recorded cap and not silently dropped. Dropping it would be the
///   easy wrong implementation: the hire succeeds, the operator believes they set a backstop, and
///   the record says nothing for the agent's whole life.
/// * **Negative, fractional, nonnumeric and out-of-range** are `-32602` from the typed decode.
///   `4294967296` is the first value past `u32::MAX` and `18446744073709551616` the first past
///   `u64::MAX`, so each is out-of-range for its own field rather than for both.
///
/// The journal is counted at three moments rather than one, so a record that arrives late cannot
/// satisfy the count: the admitted hires first, then every refusal, then the same count again.
#[test]
fn absent_zero_and_invalid_limits_remain_distinct_before_the_journal() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let mut client = Client::connect(settings.socket()).expect("connect to the daemon");

    assert!(
        recorded_limits(&fixture).is_empty(),
        "nothing is hired before this test hires"
    );

    for (what, limits) in [
        ("absent", serde_json::json!({})),
        (
            "null",
            serde_json::json!({"max_turns": null, "max_budget": null}),
        ),
    ] {
        let answer = hire_with(&mut client, &settings, &fixture, &limits);
        assert!(
            answer.error.is_none(),
            "{what} must admit as no cap: {:?}",
            answer.error
        );
    }
    let one_unit = hire_with(
        &mut client,
        &settings,
        &fixture,
        &serde_json::json!({"max_turns": 1, "max_budget": 1}),
    );
    assert!(
        one_unit.error.is_none(),
        "the smallest positive value of each field admits: {:?}",
        one_unit.error
    );
    assert_eq!(
        recorded_limits(&fixture),
        vec![
            (None, None),
            (None, None),
            (Some(1), Some(dudes_types::Usd::from_micros(1))),
        ],
        "absent and null record as no cap; a positive value records as itself"
    );

    for (what, limits) in refused_rows() {
        let refusal = hire_with(&mut client, &settings, &fixture, &limits)
            .error
            .unwrap_or_else(|| panic!("{what} must be refused"));
        assert_eq!(
            refusal.code,
            ErrorCode::InvalidParams.code(),
            "{what} is -32602: a value the field does not accept, not a state refusal: {refusal:?}"
        );
    }

    assert_eq!(
        recorded_limits(&fixture),
        vec![
            (None, None),
            (None, None),
            (Some(1), Some(dudes_types::Usd::from_micros(1))),
        ],
        "every refusal happened in front of the append: the journal is unchanged"
    );
    running.stop(ExitCause::Ended);
}
