//! Memory-condense model-summary call: admission, journaling-before-spawn, fallback, and the
//! shared exit sweep. Driven through `Ask` directly, bypassing the socket and
//! conditioned-effect router, which are exercised elsewhere (`memory_append.rs`): the condense
//! trigger and its admission live below that layer and do not need it.
#![cfg(all(unix, not(target_os = "linux")))]

use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::ports::{Answered, Ask};
use duded::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner};
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_journal::MemoryCondenseConfig;
use dudes_proto::rpc::agent::HireParams;
use dudes_proto::{AgentBudget, BudgetScope, Ceiling, PeerProvenance, Record};
use dudes_providers::launch::LaunchSpec;
use dudes_types::{AgentId, Usd};

mod common;

use common::{Fixture, SharedHost, TestClock};

fn model_output(summary: &str) -> String {
    serde_json::json!({"type":"result","subtype":"success","is_error":false,
        "result":serde_json::json!({"condensed":summary}).to_string()})
    .to_string()
}

fn captured_fixture(outcome: PredicateOutcome) -> PredicateCaptured {
    match outcome {
        PredicateOutcome::Finished {
            exit,
            output_tail,
            timed_out,
        } => PredicateCaptured::Finished {
            exit,
            stdout: output_tail.into_bytes(),
            truncated: false,
            timed_out,
        },
        PredicateOutcome::Swept => PredicateCaptured::Swept,
    }
}

#[derive(Default)]
struct CapturedScriptedRunner(common::ScriptedRunner);

impl CapturedScriptedRunner {
    fn scripted(answers: impl IntoIterator<Item = PredicateOutcome>) -> Self {
        Self(common::ScriptedRunner::scripted(answers))
    }

    fn runs(&self) -> usize {
        self.0.runs()
    }
}

impl PredicateRunner for CapturedScriptedRunner {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> std::io::Result<PredicateOutcome> {
        self.0.run(spec, timeout)
    }

    fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        stdout_cap: usize,
    ) -> std::io::Result<PredicateCaptured> {
        assert_eq!((timeout, stdout_cap), (Duration::from_secs(180), 65_536));
        self.0.run(spec, timeout).map(captured_fixture)
    }

    fn sweep(&self) {
        self.0.sweep();
    }
}

/// The number of `TurnIssued` records on the journal so far.
fn turns_issued(fixture: &Fixture) -> usize {
    fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::TurnIssued { .. }))
        .count()
}

/// A small threshold and tail so a handful of short appends trigger condensing, independent of
/// [`MemoryCondenseConfig::default`]'s production-sized 64 KiB.
fn small_config() -> MemoryCondenseConfig {
    MemoryCondenseConfig::new(1, 2).unwrap()
}

/// `agent.hire` params for a Claude agent with a zero agent-dollar ceiling: reached at zero spend,
/// so the budget gate's own `admit()` refuses the condense model call immediately,
/// with no turn, goal, or spend history needed to exercise the refusal.
fn zero_ceiling_hire_params(cwd: &std::path::Path) -> HireParams {
    HireParams {
        budget: AgentBudget {
            usd_per_day: Some(Usd::ZERO),
            tokens_per_day: None,
        },
        ..common::hire_params(cwd)
    }
}

fn hire(handle: &duded::Handle, params: HireParams) -> AgentId {
    match handle.ask(Ask::Hire(Box::new(params))).expect("the hire") {
        Answered::Hired(result) => result.agent,
        other => panic!("agent.hire answered {other:?}"),
    }
}

/// Append one memory section directly, bypassing the socket and conditioned router.
fn append(handle: &duded::Handle, agent: AgentId, text: &str) {
    match handle
        .ask(Ask::MemoryAppend {
            agent,
            payload: text.as_bytes().to_vec(),
            provenance: PeerProvenance::NoRecordedAgent,
        })
        .expect("memory.append")
    {
        Answered::MemoryAppended(_) => {}
        other => panic!("memory.append answered {other:?}"),
    }
}

/// Three 2 KiB sections: with [`small_config`]'s 1 KiB threshold and 2-section tail, the third
/// append is the one that first makes `count (3) > keep (2)` true, so `condense_memory` fires
/// exactly once across the batch — one section evicted, two left, `condense_needed` false again
/// immediately after. 2 KiB each, not a few hundred bytes, because the deterministic exact-excerpt
/// fallback keeps a 256-byte prefix and suffix per evicted section (`EXCERPT_BYTES`): a section
/// too small to exceed that combined 512 bytes cannot shrink, so `prepare_condense` correctly
/// refuses it as not useful, and a test built on sections that size would never see a condense at
/// all, model or exact.
fn push_sections_over_threshold(handle: &duded::Handle, agent: AgentId) {
    for index in 0..3 {
        let prefix = format!("section {index}: ");
        append(
            handle,
            agent,
            &format!("{prefix}{}", "x".repeat(2_048 - prefix.len())),
        );
    }
}

/// Wait until at least one `Record::MemoryCondense` for `agent` reaches the journal.
fn await_condense(fixture: &Fixture, agent: AgentId) -> Record {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(record) = fixture.records().into_iter().find(
            |record| matches!(record, Record::MemoryCondense { agent: who, .. } if *who == agent),
        ) {
            return record;
        }
        assert!(
            Instant::now() < deadline,
            "no MemoryCondense reached the journal within 10s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Whether a published condense summary's immutable file is the deterministic exact-excerpt kind
/// (`"kind":"exact"`) or the model kind (`"kind":"model"`) — read directly off disk rather than
/// through a parser this test would then have to trust is faithful to the one under test.
fn summary_kind(fixture: &Fixture, record: &Record) -> String {
    let Record::MemoryCondense { summary_ref, .. } = record else {
        panic!("not a MemoryCondense record: {record:?}")
    };
    let bytes = std::fs::read(fixture.state.path().join(summary_ref))
        .unwrap_or_else(|error| panic!("reading {}: {error}", summary_ref.display()));
    let text = String::from_utf8(bytes).expect("the summary file is UTF-8 JSON");
    if text.contains("\"kind\":\"exact\"") {
        "exact".to_owned()
    } else if text.contains("\"kind\":\"model\"") {
        "model".to_owned()
    } else {
        panic!("summary file names neither content kind: {text}")
    }
}

/// A runner that panics unless at least one `MemoryCondenseModelAdmitted { allowed: true, .. }`
/// is already durable in `fixture`'s journal the moment it is asked to run — the positive half of
/// "Journaled before it spawns": a mutant that spawned the child before (or instead
/// of) journaling the admission turns this run red, not merely slow. Scoped to "any agent" rather
/// than one in particular: every test that uses it hires exactly one.
struct AssertAdmittedFirst {
    journal_dir: std::path::PathBuf,
    inner: CapturedScriptedRunner,
}

impl AssertAdmittedFirst {
    /// Whether the journal already names an admitted call, read fresh each poll. A short poll
    /// rather than a single read: `run()` executes on its own thread, with no barrier against the
    /// daemon thread's own concurrent progress, so a single immediate read can beat an fsync that
    /// is already in flight. A bounded poll keeps that race from being mistaken for the bug it
    /// exists to catch, while still failing — reliably, not eventually — when the record is never
    /// written at all, which is what every mutant this test names actually produces.
    fn admitted(&self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let found = dudes_journal::Reader::new(&self.journal_dir)
                .iter_from(dudes_types::Lsn::ZERO)
                .expect("open the journal for reading")
                .map(|item| item.expect("read a journal entry").1)
                .any(|entry| {
                    matches!(
                        entry.record,
                        Record::MemoryCondenseModelAdmitted { allowed: true, .. }
                    )
                });
            if found || Instant::now() >= deadline {
                return found;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl PredicateRunner for AssertAdmittedFirst {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> std::io::Result<PredicateOutcome> {
        assert!(
            self.admitted(),
            "the model child was asked to run with no admitted decision ever reaching the journal"
        );
        self.inner.run(spec, timeout)
    }

    fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        stdout_cap: usize,
    ) -> std::io::Result<PredicateCaptured> {
        assert!(
            self.admitted(),
            "the capture is not allowed before durable admission"
        );
        self.inner.run_captured(spec, timeout, stdout_cap)
    }

    fn run_discarding_output(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
    ) -> std::io::Result<PredicateOutcome> {
        self.inner.run_discarding_output(spec, timeout)
    }

    fn sweep(&self) {
        self.inner.sweep();
    }
}

/// A runner that sleeps past its caller's patience before answering, so a test can stop the
/// daemon while the call is still "in flight" and prove nothing in that window required the
/// daemon to wait for it, and counts its own sweeps so a test can prove the exit path reaches it.
struct Sleepy {
    for_how_long: Duration,
    then: PredicateOutcome,
    sweeps: std::sync::atomic::AtomicUsize,
}

impl Sleepy {
    fn new(for_how_long: Duration, then: PredicateOutcome) -> Self {
        Self {
            for_how_long,
            then,
            sweeps: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn sweeps(&self) -> usize {
        self.sweeps.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl PredicateRunner for Sleepy {
    fn run(&self, _spec: &LaunchSpec, _timeout: Duration) -> std::io::Result<PredicateOutcome> {
        std::thread::sleep(self.for_how_long);
        Ok(self.then.clone())
    }

    fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        stdout_cap: usize,
    ) -> std::io::Result<PredicateCaptured> {
        assert_eq!((timeout, stdout_cap), (Duration::from_secs(180), 65_536));
        self.run(spec, timeout).map(captured_fixture)
    }

    fn sweep(&self) {
        self.sweeps
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A runner whose call stays in flight until the test releases it, so "in flight" is a state the
/// test holds rather than a window it races. A dropped release handle also ends the call.
struct Gated {
    then: PredicateOutcome,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    runs: std::sync::atomic::AtomicUsize,
}

impl Gated {
    fn new(then: PredicateOutcome) -> (Self, std::sync::mpsc::Sender<()>) {
        let (release_tx, release) = std::sync::mpsc::channel();
        let gated = Self {
            then,
            release: std::sync::Mutex::new(release),
            runs: std::sync::atomic::AtomicUsize::new(0),
        };
        (gated, release_tx)
    }

    fn runs(&self) -> usize {
        self.runs.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl PredicateRunner for Gated {
    fn run(&self, _spec: &LaunchSpec, _timeout: Duration) -> std::io::Result<PredicateOutcome> {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = self
            .release
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv();
        Ok(self.then.clone())
    }

    fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        stdout_cap: usize,
    ) -> std::io::Result<PredicateCaptured> {
        assert_eq!((timeout, stdout_cap), (Duration::from_secs(180), 65_536));
        self.run(spec, timeout).map(captured_fixture)
    }

    fn sweep(&self) {}
}

fn seams_for(
    settings: &duded::Settings,
    predicates: Arc<impl PredicateRunner>,
) -> duded::settings::Seams<SharedHost, impl PredicateRunner, TestClock, common::FixtureRng> {
    common::seams(
        settings,
        SharedHost::new(),
        predicates,
        Arc::new(TestClock::new()),
        Witness::default(),
    )
}

struct CaptureRoute {
    legacy: std::sync::atomic::AtomicUsize,
    captures: std::sync::Mutex<Vec<(Duration, usize)>>,
    result: Result<PredicateCaptured, std::io::ErrorKind>,
}

impl CaptureRoute {
    fn output() -> String {
        serde_json::json!({
            "type":"result","subtype":"success","is_error":false,
            "result":r#"{"condensed":"a bounded captured summary"}"#
        })
        .to_string()
    }
}

impl PredicateRunner for CaptureRoute {
    fn run(&self, _spec: &LaunchSpec, _timeout: Duration) -> std::io::Result<PredicateOutcome> {
        self.legacy
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(PredicateOutcome::Finished {
            exit: Some(0),
            output_tail: Self::output(),
            timed_out: false,
        })
    }

    fn run_captured(
        &self,
        _spec: &LaunchSpec,
        timeout: Duration,
        stdout_cap: usize,
    ) -> std::io::Result<PredicateCaptured> {
        self.captures.lock().unwrap().push((timeout, stdout_cap));
        self.result.clone().map_err(Into::into)
    }

    fn sweep(&self) {}
}

#[cfg(not(target_os = "linux"))]
#[test]
fn condense_uses_bounded_raw_capture_instead_of_the_interleaved_tail() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.condense_model_enabled = true;
    let runner = Arc::new(CaptureRoute {
        legacy: std::sync::atomic::AtomicUsize::new(0),
        captures: std::sync::Mutex::new(Vec::new()),
        result: Ok(PredicateCaptured::Finished {
            exit: Some(0),
            stdout: CaptureRoute::output().into_bytes(),
            truncated: false,
            timed_out: false,
        }),
    });
    let daemon = boot::boot_with_memory_config(
        &settings,
        seams_for(&settings, Arc::clone(&runner)),
        small_config(),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let agent = hire(&running.handle, common::hire_params(fixture.state.path()));
    push_sections_over_threshold(&running.handle, agent);
    let condensed = await_condense(&fixture, agent);
    running.stop(ExitCause::Ended);
    assert_eq!(summary_kind(&fixture, &condensed), "model");
    assert_eq!(
        *runner.captures.lock().unwrap(),
        [(Duration::from_secs(180), 65_536)]
    );
    assert_eq!(runner.legacy.load(std::sync::atomic::Ordering::SeqCst), 0);
}

fn assert_raw_capture_fallback(result: Result<PredicateCaptured, std::io::ErrorKind>) {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.condense_model_enabled = true;
    let runner = Arc::new(CaptureRoute {
        legacy: std::sync::atomic::AtomicUsize::new(0),
        captures: std::sync::Mutex::new(Vec::new()),
        result,
    });
    let daemon = boot::boot_with_memory_config(
        &settings,
        seams_for(&settings, Arc::clone(&runner)),
        small_config(),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let agent = hire(&running.handle, common::hire_params(fixture.state.path()));
    push_sections_over_threshold(&running.handle, agent);
    let condensed = await_condense(&fixture, agent);
    running.stop(ExitCause::Ended);
    assert_eq!(summary_kind(&fixture, &condensed), "exact");
    assert_eq!(
        *runner.captures.lock().unwrap(),
        [(Duration::from_secs(180), 65_536)]
    );
    assert_eq!(runner.legacy.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn a_valid_prefix_with_truncation_is_never_salvaged() {
    assert_raw_capture_fallback(Ok(PredicateCaptured::Finished {
        exit: Some(0),
        stdout: CaptureRoute::output().into_bytes(),
        truncated: true,
        timed_out: false,
    }));
}

#[test]
fn a_capture_larger_than_the_requested_cap_is_refused() {
    let mut stdout = CaptureRoute::output().into_bytes();
    stdout.resize(65_537, b' ');
    assert_raw_capture_fallback(Ok(PredicateCaptured::Finished {
        exit: Some(0),
        stdout,
        truncated: false,
        timed_out: false,
    }));
}

#[test]
fn invalid_utf8_in_a_success_shape_uses_exact_excerpts() {
    let mut stdout = CaptureRoute::output().into_bytes();
    let offset = stdout
        .windows(9)
        .position(|bytes| bytes == b"a bounded")
        .unwrap();
    stdout[offset] = 0xff;
    assert_raw_capture_fallback(Ok(PredicateCaptured::Finished {
        exit: Some(0),
        stdout,
        truncated: false,
        timed_out: false,
    }));
}

#[test]
fn swept_and_unsupported_captures_use_exact_excerpts() {
    assert_raw_capture_fallback(Ok(PredicateCaptured::Swept));
    assert_raw_capture_fallback(Err(std::io::ErrorKind::Unsupported));
}

/// **Admission refused over a ceiling falls back to exact excerpts, and the model child is never
/// asked to run.** Names `SKIP_ADMISSION_CHECK`: an implementation that treated every condense as
/// admitted regardless of `admit()`'s own verdict would still spawn the model child here, so
/// `runner.runs() == 0` is the assertion that mutant turns red, not merely the published content.
#[test]
fn admission_refused_over_a_ceiling_falls_back_to_exact_excerpts() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.condense_model_enabled = true;
    let runner = Arc::new(CapturedScriptedRunner::default());
    let daemon = boot::boot_with_memory_config(
        &settings,
        seams_for(&settings, Arc::clone(&runner)),
        small_config(),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    let agent = hire(
        &running.handle,
        zero_ceiling_hire_params(fixture.state.path()),
    );
    push_sections_over_threshold(&running.handle, agent);

    let condensed = await_condense(&fixture, agent);
    assert_eq!(summary_kind(&fixture, &condensed), "exact");
    assert_eq!(
        runner.runs(),
        0,
        "a refused admission must never spawn the model child"
    );

    let admitted = fixture
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::MemoryCondenseModelAdmitted {
                agent: who,
                allowed,
                ceiling,
                ..
            } if who == agent => Some((allowed, ceiling)),
            _ => None,
        })
        .expect("a MemoryCondenseModelAdmitted record for this agent");
    assert!(!admitted.0, "the admission decision itself");
    assert_eq!(
        admitted.1,
        Some(Ceiling {
            scope: BudgetScope::Agent,
            usd: Some(Usd::ZERO),
            tokens: None,
        }),
        "the ceiling that refused it"
    );

    running.stop(ExitCause::Ended);
}

/// **The admission decision is journaled before the child spawns.** Names
/// `SPAWN_BEFORE_JOURNAL`: [`AssertAdmittedFirst::run`] panics (failing this test) the moment it
/// is invoked without an already-durable `MemoryCondenseModelAdmitted { allowed: true, .. }`,
/// which is exactly what a mutant reordering the spawn ahead of the journal append would produce.
#[test]
fn the_admission_decision_is_journaled_before_the_child_spawns() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.condense_model_enabled = true;
    let runner = Arc::new(AssertAdmittedFirst {
        journal_dir: fixture.journal_dir(),
        inner: CapturedScriptedRunner::scripted([common::passes()]),
    });
    let daemon = boot::boot_with_memory_config(
        &settings,
        seams_for(&settings, Arc::clone(&runner)),
        small_config(),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    let agent = hire(&running.handle, common::hire_params(fixture.state.path()));
    push_sections_over_threshold(&running.handle, agent);
    await_condense(&fixture, agent);
    assert!(
        runner.inner.runs() >= 1,
        "the model child ran at least once"
    );

    running.stop(ExitCause::Ended);
}

/// Wait until at least one admitted `MemoryCondenseModelAdmitted` reaches the journal.
fn await_admitted(fixture: &Fixture) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if fixture.records().iter().any(|record| {
            matches!(
                record,
                Record::MemoryCondenseModelAdmitted { allowed: true, .. }
            )
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no admitted MemoryCondenseModelAdmitted reached the journal within 10s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One scripted model-call outcome, exercised end to end, always falls back to exact excerpts.
/// Each of the three calls below names the mutant it kills: `IGNORE_EXIT_CODE` (a nonzero exit
/// treated as success), `IGNORE_TIMEOUT_FLAG` (`timed_out: true` treated as success), and
/// `TRUST_UNPARSED_OUTPUT` (unparseable text published as though it were a validated summary).
fn assert_falls_back_to_exact_excerpts(outcome: PredicateOutcome) {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.condense_model_enabled = true;
    let runner = Arc::new(CapturedScriptedRunner::scripted([outcome]));
    let daemon = boot::boot_with_memory_config(
        &settings,
        seams_for(&settings, Arc::clone(&runner)),
        small_config(),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    let agent = hire(&running.handle, common::hire_params(fixture.state.path()));
    push_sections_over_threshold(&running.handle, agent);

    let condensed = await_condense(&fixture, agent);
    assert_eq!(summary_kind(&fixture, &condensed), "exact");
    assert_eq!(runner.runs(), 1, "the model child ran exactly once");

    running.stop(ExitCause::Ended);
}

#[test]
fn a_nonzero_exit_falls_back_to_exact_excerpts() {
    assert_falls_back_to_exact_excerpts(PredicateOutcome::Finished {
        exit: Some(1),
        output_tail: model_output("this must not be trusted"),
        timed_out: false,
    });
}

#[test]
fn a_timed_out_call_falls_back_to_exact_excerpts() {
    assert_falls_back_to_exact_excerpts(PredicateOutcome::Finished {
        exit: Some(0),
        output_tail: model_output("this must not be trusted either"),
        timed_out: true,
    });
}

#[test]
fn unparseable_output_falls_back_to_exact_excerpts() {
    assert_falls_back_to_exact_excerpts(PredicateOutcome::Finished {
        exit: Some(0),
        output_tail: "the model printed prose, not the strict JSON it was asked for".to_owned(),
        timed_out: false,
    });
}

#[test]
fn an_admitted_call_left_in_flight_at_a_crash_gets_exact_catch_up() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.condense_model_enabled = true;

    let sleepy = Arc::new(Sleepy::new(Duration::from_secs(5), common::passes()));
    let daemon = boot::boot_with_memory_config(
        &settings,
        seams_for(&settings, Arc::clone(&sleepy)),
        small_config(),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let agent = hire(&running.handle, common::hire_params(fixture.state.path()));
    push_sections_over_threshold(&running.handle, agent);

    // The admission is durable well before the 5s sleep ends; stopping here is the "crash" —
    // the call never gets to answer this daemon.
    await_admitted(&fixture);
    let stopped_at = Instant::now();
    running.stop(ExitCause::Ended);
    assert!(
        stopped_at.elapsed() < Duration::from_secs(2),
        "stopping the daemon must not wait for the in-flight model call"
    );
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::MemoryCondense { .. })),
        "the abandoned call must not have published anything"
    );

    // Reboot over the same journal with a runner that actually answers quickly, and nudge the
    // catch-up queue with one more append — `condense_needed` is still true, so this agent is
    // already in `memory_condenses` from `boot_configured`'s own replay-derived scan.
    let fast = Arc::new(CapturedScriptedRunner::scripted([
        PredicateOutcome::Finished {
            exit: Some(0),
            output_tail: model_output("a fresh model summary"),
            timed_out: false,
        },
    ]));
    let daemon = boot::boot_with_memory_config(
        &settings,
        seams_for(&settings, Arc::clone(&fast)),
        small_config(),
    )
    .expect("reboot");
    let mut running = common::start(daemon);
    append(
        &running.handle,
        agent,
        "one more section to re-trigger the tick",
    );
    let condensed = await_condense(&fixture, agent);
    assert_eq!(summary_kind(&fixture, &condensed), "exact");
    assert_eq!(
        fast.runs(),
        0,
        "a bare admission already consumed this turn allowance"
    );

    running.stop(ExitCause::Ended);
}

/// **A condense model call is swept by the same exit path a predicate child is** (invariant 10):
/// `spawn_condense_model` routes through `self.ctx.predicates`, the one `Arc` the daemon already
/// reaches from its shutdown sequence and that `dudes_host::OneShot`'s own tests
/// (`crates/dudes-host/src/oneshot.rs`) prove kills a real child's process group. This test proves
/// the structural half — the same runner instance sees the sweep — without re-proving process-
/// group mechanics `OneShot` already owns.
#[test]
fn stopping_the_daemon_sweeps_an_in_flight_condense_model_call() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.condense_model_enabled = true;
    let sleepy = Arc::new(Sleepy::new(Duration::from_secs(5), common::passes()));
    let daemon = boot::boot_with_memory_config(
        &settings,
        seams_for(&settings, Arc::clone(&sleepy)),
        small_config(),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let agent = hire(&running.handle, common::hire_params(fixture.state.path()));
    push_sections_over_threshold(&running.handle, agent);
    await_admitted(&fixture);

    running.stop(ExitCause::Ended);

    assert!(
        sleepy.sweeps() >= 1,
        "the daemon's exit sequence must sweep the runner the condense call used"
    );
}

/// **A second trigger while a call is in flight publishes nothing early.** A trigger that lands
/// while an earlier model call for the same agent is still in flight must not fall through to the
/// deterministic path immediately. The fall-through would be reached not by a second model call
/// (the per-turn rate limit already blocks that, proven separately by
/// `at_most_one_model_call_per_turn_marker`, and still would here, since both triggers share one
/// turn-less marker) but by the `if` around the model attempt being false, which sends the second
/// trigger straight to `finish_condense_deterministic`. That publishes ahead of the admitted,
/// charged model call still asleep, stranding it: the model call's own later answer then loses
/// the shared predecessor check and is silently dropped, wasting what it was admitted to spend.
/// Names `IGNORE_INFLIGHT_GUARD`: removing the `memory_condense_inflight` check at the top of
/// `condense_memory` turns this red, because the second trigger then reaches that fallthrough
/// instead of returning immediately.
#[test]
fn a_second_trigger_while_in_flight_publishes_nothing_early() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.condense_model_enabled = true;
    let (gated, release) = Gated::new(PredicateOutcome::Finished {
        exit: Some(0),
        output_tail: model_output("first model summary"),
        timed_out: false,
    });
    let gated = Arc::new(gated);
    let daemon = boot::boot_with_memory_config(
        &settings,
        seams_for(&settings, Arc::clone(&gated)),
        small_config(),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let agent = hire(&running.handle, common::hire_params(fixture.state.path()));

    push_sections_over_threshold(&running.handle, agent);
    await_admitted(&fixture);
    // The call is admitted and held by the gate, so it is in flight for as long as this test
    // chooses. Each append is answered after the loop handled its trigger, so once this returns
    // the second trigger has been decided.
    push_sections_over_threshold(&running.handle, agent);

    let early = fixture
        .records()
        .into_iter()
        .filter(|record| matches!(record, Record::MemoryCondense { .. }))
        .count();
    assert_eq!(
        early, 0,
        "a second trigger while a call is in flight must not publish the deterministic \
         fallback ahead of it"
    );

    release.send(()).expect("the in-flight call is waiting");
    await_condense(&fixture, agent);
    running.stop(ExitCause::Ended);
    assert_eq!(
        gated.runs(),
        1,
        "a second trigger while a call is in flight must not start a second model call"
    );
    let published = fixture
        .records()
        .into_iter()
        .filter(|record| matches!(record, Record::MemoryCondense { .. }))
        .count();
    assert_eq!(
        published, 1,
        "exactly the in-flight call's own summary is ever published"
    );
}

/// **At most one admitted model call per `TurnIssued` for an agent.** Names
/// `IGNORE_TURN_RATE_LIMIT`: without the per-turn marker, a second threshold crossing for the
/// same (turn-less, so same-marker) agent would spend a second model call purely because the
/// agent wrote more filler to its own memory; with the marker, the second crossing is rate-
/// limited to the deterministic fallback.
#[test]
fn at_most_one_model_call_per_turn_marker() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.condense_model_enabled = true;
    let runner = Arc::new(CapturedScriptedRunner::scripted([
        PredicateOutcome::Finished {
            exit: Some(0),
            output_tail: model_output("first model summary"),
            timed_out: false,
        },
    ]));
    let daemon = boot::boot_with_memory_config(
        &settings,
        seams_for(&settings, Arc::clone(&runner)),
        small_config(),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let agent = hire(&running.handle, common::hire_params(fixture.state.path()));

    push_sections_over_threshold(&running.handle, agent);
    let first = await_condense(&fixture, agent);
    assert_eq!(summary_kind(&fixture, &first), "model");
    assert_eq!(
        runner.runs(),
        1,
        "the first trigger spent exactly one model call"
    );

    push_sections_over_threshold(&running.handle, agent);
    assert_eq!(
        runner.runs(),
        1,
        "a second condense under the same turn marker must not spend a second model call"
    );
    let second = fixture
        .records()
        .into_iter()
        .rfind(|record| matches!(record, Record::MemoryCondense { .. }))
        .expect("a second MemoryCondense");
    assert_eq!(summary_kind(&fixture, &second), "exact");

    running.stop(ExitCause::Ended);
}

/// **A model-condense admission survives a restart within the same still-open turn.** Regression
/// test: the original in-memory marker reset to nothing at boot, so a
/// restart mid-turn let the same open turn pay for a second call. This test never ends the turn
/// it issues, so the reboot resumes it exactly as described. Names
/// `IGNORE_JOURNALED_RATE_LIMIT`: forcing `model_condense_already_admitted_this_turn` to always
/// answer `false` turns this red, because the reboot would then spend a second model call under
/// the turn that had already spent its one.
#[test]
fn a_restart_within_the_same_turn_spends_no_second_model_call() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.condense_model_enabled = true;
    let runner = Arc::new(CapturedScriptedRunner::scripted([
        PredicateOutcome::Finished {
            exit: Some(0),
            output_tail: model_output("first model summary"),
            timed_out: false,
        },
    ]));
    let daemon = boot::boot_with_memory_config(
        &settings,
        seams_for(&settings, Arc::clone(&runner)),
        small_config(),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let agent = hire(&running.handle, common::hire_params(fixture.state.path()));
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    fixture.await_turn();

    push_sections_over_threshold(&running.handle, agent);
    let first = await_condense(&fixture, agent);
    assert_eq!(summary_kind(&fixture, &first), "model");
    assert_eq!(
        runner.runs(),
        1,
        "the first trigger spent exactly one model call"
    );
    let turns_before_restart = turns_issued(&fixture);
    assert_eq!(
        turns_before_restart, 1,
        "control: still the one turn this test issued"
    );

    running.stop(ExitCause::Ended);

    // Reboot over the same journal. The turn this test issued is never ended, so replay resumes
    // the identical, still-open turn — exactly the "restart mid-turn" case.
    let runner = Arc::new(CapturedScriptedRunner::scripted([
        PredicateOutcome::Finished {
            exit: Some(0),
            output_tail: model_output("a second model summary, if this were wrongly allowed"),
            timed_out: false,
        },
    ]));
    let daemon = boot::boot_with_memory_config(
        &settings,
        seams_for(&settings, Arc::clone(&runner)),
        small_config(),
    )
    .expect("reboot");
    let mut running = common::start(daemon);
    assert_eq!(
        turns_issued(&fixture),
        turns_before_restart,
        "control: the reboot alone must not issue a second turn"
    );

    push_sections_over_threshold(&running.handle, agent);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        runner.runs(),
        0,
        "a restart within the same still-open turn must not spend a second model call"
    );
    let second = fixture
        .records()
        .into_iter()
        .rfind(|record| matches!(record, Record::MemoryCondense { .. }))
        .expect("a second MemoryCondense, from the deterministic fallback");
    assert_eq!(summary_kind(&fixture, &second), "exact");

    running.stop(ExitCause::Ended);
}

/// **A model-condense admission is not reset by switching goals and back.** Regression test: the original in-memory marker was read through the agent's *current*
/// goal's own turn history, so pointing the agent at a different goal and back read a goal with
/// no turn of its own in between, making the one stored marker — still the first goal's turn —
/// appear not to match, wrongly allowing a second admission for the very same still-open turn.
/// This design never reads through a goal at all, so it cannot be fooled this way. Shares its
/// named mutant, `IGNORE_JOURNALED_RATE_LIMIT`, with the restart test above: the one check both guard.
#[test]
fn a_goal_switch_and_back_does_not_reset_the_rate_limit() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.condense_model_enabled = true;
    let runner = Arc::new(CapturedScriptedRunner::scripted([
        PredicateOutcome::Finished {
            exit: Some(0),
            output_tail: model_output("first model summary"),
            timed_out: false,
        },
    ]));
    let daemon = boot::boot_with_memory_config(
        &settings,
        seams_for(&settings, Arc::clone(&runner)),
        small_config(),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let agent = hire(&running.handle, common::hire_params(fixture.state.path()));
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    fixture.await_turn();

    push_sections_over_threshold(&running.handle, agent);
    let first = await_condense(&fixture, agent);
    assert_eq!(summary_kind(&fixture, &first), "model");
    assert_eq!(
        runner.runs(),
        1,
        "the first trigger spent exactly one model call"
    );

    // Switch away and back, twice, without ending the still-open turn above. Each `GoalSet`
    // mints a fresh goal and points the agent at it; `decide` refuses to issue a turn while one
    // is already open (`AgentRow::open_turn`), so no `TurnIssued` fires again here — only
    // `agent.goal` moves, back and forth.
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/false"]),
    );
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    assert_eq!(
        turns_issued(&fixture),
        1,
        "control: switching goals while a turn is open must not issue a second one"
    );

    push_sections_over_threshold(&running.handle, agent);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        runner.runs(),
        1,
        "a goal switched away and back must not re-admit the same still-open turn"
    );
    let second = fixture
        .records()
        .into_iter()
        .rfind(|record| matches!(record, Record::MemoryCondense { .. }))
        .expect("a second MemoryCondense, from the deterministic fallback");
    assert_eq!(summary_kind(&fixture, &second), "exact");

    running.stop(ExitCause::Ended);
}
