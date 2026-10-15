//! A real daemon/host child-pipe test for bounded, attributable Managed text.
#![cfg(unix)]

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use duded::output::{Loss, OutputRegistry, OutputState, Unavailable};
use duded::settings::Seams;
use duded::startup::Witness;
use duded::{ChildEnds, ChildTurn, ExitCause, boot};
use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_proto::{ExitStatus, Record};
use dudes_providers::adapter::{
    Adapter, AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
};
use dudes_providers::claude::SESSION_ACQUISITION;
use dudes_providers::claude::stream::TurnStream;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_providers::stream::TurnReader;
use dudes_supervisor::host::{AdoptionKey, KillOutcome, Liveness, ProcessHost, SpawnResult};
use dudes_supervisor::testing::FakeRng;
use dudes_types::{AgentId, HostBoot, ModelSelection, Provider, Tier, TurnId};

mod common;
use common::{Fixture, ScriptedRunner, TestClock};

const HELPER: &str = "DUDES_CAPTURE_CHILD_HELPER";
const CHILD_LABEL: &str = "DUDES_CAPTURE_CHILD_LABEL";
const CHILD_SESSION: &str = "DUDES_CAPTURE_CHILD_SESSION";
const CHILD_DIR: &str = "DUDES_CAPTURE_CHILD_DIR";

#[test]
fn the_helper_entry_point() {
    if std::env::var_os(HELPER).is_none() {
        return;
    }
    let label = std::env::var(CHILD_LABEL).expect("label");
    let session = if matches!(label.as_str(), "owner" | "rejected") {
        "00000000-0000-4000-8000-000000000099".to_owned()
    } else if matches!(label.as_str(), "overcap" | "overcap-live") {
        "s".repeat(257)
    } else {
        std::env::var(CHILD_SESSION).expect("session")
    };
    let dir = PathBuf::from(std::env::var_os(CHILD_DIR).expect("dir"));
    let mut out = std::io::stdout().lock();
    let assistant = |text: &str| {
        format!(
            r#"{{"type":"assistant","session_id":"{session}","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    };
    if label == "overcap-live" {
        held_overcap_child(&mut out, &dir, &session);
        std::process::exit(0);
    }
    if matches!(label.as_str(), "short" | "owner" | "rejected" | "overcap") {
        writeln!(out, "{}", assistant(&format!("{label}-owned-text"))).expect("short text");
        writeln!(
            out,
            r#"{{"type":"system","subtype":"init","session_id":"{session}","tools":[]}}"#
        )
        .expect("short init");
        out.flush().expect("short flush");
        std::process::exit(0);
    }
    if label == "no-init" {
        writeln!(out, "{}", assistant("unconfirmed-text")).expect("unconfirmed text");
        out.flush().expect("no-init flush");
        std::process::exit(0);
    }
    if label.starts_with("bulk-") {
        writeln!(
            out,
            r#"{{"type":"system","subtype":"init","session_id":"{session}","tools":[]}}"#
        )
        .expect("init");
        for index in 0..16 {
            let separator_bytes = usize::from(index > 0);
            writeln!(out, "{}", assistant(&"q".repeat(4_096 - separator_bytes)))
                .expect("bulk text");
        }
        out.flush().expect("bulk flush");
        std::process::exit(0);
    }
    // An assistant line beats init/registration to exercise the full-key pending gate.
    writeln!(out, "{}", assistant(&format!("{label}-early"))).expect("early");
    out.flush().expect("early flush");
    std::fs::write(dir.join("started"), b"ready").expect("started marker");
    await_file(&dir.join("release-init"));
    writeln!(
        out,
        r#"{{"type":"system","subtype":"init","session_id":"{session}","tools":[]}}"#
    )
    .expect("init");
    writeln!(out, "{}", assistant(&format!("{label}-live"))).expect("live");
    out.flush().expect("live flush");
    await_file(&dir.join("release-wrong"));
    writeln!(out, r#"{{"type":"assistant","session_id":"wrong","message":{{"role":"assistant","content":[{{"type":"text","text":"{label}-WRONG-SESSION"}}]}}}}"#).expect("wrong session");
    writeln!(out, r#"{{"type":"assistant","session_id":"{session}","isSynthetic":true,"message":{{"role":"assistant","content":[{{"type":"text","text":"{label}-synthetic"}}]}}}}"#).expect("synthetic");
    writeln!(out, r#"{{"type":"assistant","session_id":"{session}","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"{label}-thought"}},{{"type":"tool_use","name":"Read","input":{{"secret":"{label}-tool"}}}}]}}}}"#).expect("thinking/tool");
    writeln!(out, r#"{{"type":"user","session_id":"{session}","message":{{"role":"user","content":[{{"type":"text","text":"{label}-user"}}]}}}}"#).expect("user");
    writeln!(out, r#"{{"type":"system","subtype":"hook_response","session_id":"{session}","output":"{label}-hook"}}"#).expect("hook");
    writeln!(out, r#"{{"type":"result","session_id":"{session}","subtype":"success","is_error":false,"result":"{label}-result"}}"#).expect("result");
    writeln!(
        out,
        r#"{{"type":"unknown","session_id":"{session}","text":"{label}-unknown"}}"#
    )
    .expect("unknown");
    if label == "B" {
        writeln!(
            out,
            "{}",
            assistant(&format!("B-OVERSIZE-{}", "x".repeat(1_048_576)))
        )
        .expect("oversized source line");
        writeln!(out, "{}", assistant("B-last")).expect("B completed marker");
    }
    if label == "A" {
        writeln!(out, "{}", assistant(&"x".repeat(1_048_576))).expect("overlong source line");
        for _ in 0..132 {
            writeln!(out, "{}", assistant(&"é".repeat(500))).expect("bounded ring");
        }
    }
    out.flush().expect("content flush");
    std::fs::write(dir.join("wrote"), b"ready").expect("wrote marker");
    await_file(&dir.join("release-exit"));
    std::process::exit(0);
}

fn held_overcap_child(out: &mut impl std::io::Write, dir: &Path, session: &str) {
    writeln!(out, r#"{{"type":"assistant","session_id":"{session}","message":{{"role":"assistant","content":[{{"type":"text","text":"never-retain-overcap-text"}}]}}}}"#).expect("overcap text");
    out.flush().expect("pre-init flush");
    std::fs::write(dir.join("started"), b"ready").expect("started marker");
    await_file(&dir.join("release-init"));
    writeln!(
        out,
        r#"{{"type":"system","subtype":"init","session_id":"{session}","tools":[]}}"#
    )
    .expect("overcap init");
    out.flush().expect("init flush");
    await_file(&dir.join("release-exit"));
}

fn await_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for local child release"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[derive(Debug)]
struct LocalAdapter;

impl Adapter for LocalAdapter {
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
        session: &TurnSession,
        _selection: &ModelSelection,
    ) -> Result<LaunchSpec, dudes_providers::launch::LaunchError> {
        let session = session
            .id()
            .ok_or(dudes_providers::launch::LaunchError::SessionModeMismatch)?;
        let label = ctx
            .cwd
            .file_name()
            .expect("fixture label")
            .to_string_lossy()
            .into_owned();
        Ok(LaunchSpec {
            argv: [
                OsString::from(std::env::current_exe().expect("test binary")),
                "--exact".into(),
                "the_helper_entry_point".into(),
                "--quiet".into(),
            ]
            .into(),
            env: [
                EnvVar::new(HELPER, "1"),
                EnvVar::new(CHILD_LABEL, label),
                EnvVar::new(CHILD_SESSION, session.as_str()),
                EnvVar::new(CHILD_DIR, ctx.cwd.as_os_str()),
            ]
            .into(),
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
    fn billing_env_vars(&self) -> &'static [&'static str] {
        &[]
    }
}

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

/// A real host whose return from spawn waits for its real watcher to report EOF.
/// This fixes the interleaving in the test while retaining the production child path.
struct EofBeforeRegister {
    inner: RealHost,
    output: Arc<OutputRegistry>,
    fired: Arc<AtomicUsize>,
}

impl ProcessHost for EofBeforeRegister {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        let prior = self.output.usage().finished;
        let result = self.inner.spawn(spec, install);
        if matches!(result, SpawnResult::Spawned(_)) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.output.usage().finished == prior {
                assert!(
                    Instant::now() < deadline,
                    "real stdout watcher did not finish before host return"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            self.fired.fetch_add(1, Ordering::SeqCst);
        }
        result
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
    fn shutdown(&mut self) -> std::io::Result<()> {
        self.inner.shutdown()
    }
}

fn await_output(
    output: &OutputRegistry,
    agent: AgentId,
    predicate: impl Fn(&OutputState) -> bool,
) -> OutputState {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut snapshot = output.snapshot(agent);
    loop {
        if predicate(&snapshot.state) {
            return snapshot.state;
        }
        assert!(
            Instant::now() < deadline,
            "output did not satisfy assertion: {:?}",
            snapshot.state
        );
        snapshot = output.wait_changed(agent, snapshot.revision, Duration::from_millis(50));
    }
}

fn text(state: &OutputState) -> String {
    match state {
        OutputState::Ready { chunks, .. } => chunks.iter().map(|c| c.text.as_str()).collect(),
        _ => String::new(),
    }
}

#[test]
fn real_short_child_eof_before_registration_keeps_durably_bound_ended_text() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let dir = fixture.state.path().join("short");
    std::fs::create_dir(&dir).expect("short child work directory");
    let children = ChildEnds::new();
    let output = children.output();
    let fired = Arc::new(AtomicUsize::new(0));
    let host = EofBeforeRegister {
        inner: RealHost::watching(Arc::new(HostStreams(Arc::clone(&children)))),
        output: Arc::clone(&output),
        fired: Arc::clone(&fired),
    };
    let seams = Seams {
        host,
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(ScriptedRunner::scripted([common::passes()])),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
        resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
        adapters: [Box::new(LocalAdapter) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children: Arc::clone(&children),
        witness: Witness::default(),
    };
    let mut running = common::start(boot(&fixture.settings(), seams).expect("boot"));
    let agent = common::hire(&running.handle, &dir);
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let (turn, session) =
        fixture.await_record("short child accepted SessionLearned", |r| match r {
            Record::SessionLearned {
                agent: found,
                turn,
                session,
            } if *found == agent => Some((*turn, session.clone())),
            _ => None,
        });
    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "real watcher EOF preceded Spawn return"
    );
    fixture.await_record("short child accepted TurnEnded", |r| matches!(r, Record::TurnEnded { agent: found, turn: seen, .. } if *found == agent && *seen == turn).then_some(()));
    let ended = await_output(
        &output,
        agent,
        |s| matches!(s, OutputState::Ready { turn: seen, ended: true, .. } if *seen == turn),
    );
    assert_eq!(text(&ended), "short-owned-text");
    let key = fixture
        .records()
        .iter()
        .find_map(|r| match r {
            Record::TurnSpawned {
                turn: seen,
                pid,
                start_time,
                boot,
                ..
            } if *seen == turn => Some(AdoptionKey {
                pid: *pid,
                start_time: *start_time,
                boot: boot.clone(),
            }),
            _ => None,
        })
        .expect("durable spawned key");
    let mut late = children.watch_with(Some(&key), Box::new(TurnStream::new()));
    late.line(&format!(r#"{{"type":"assistant","session_id":"{}","message":{{"role":"assistant","content":[{{"type":"text","text":"late-after-eof"}}]}}}}"#, session.as_str()));
    assert_eq!(text(&output.snapshot(agent).state), "short-owned-text");
    assert!(output.usage().refused > 0, "late callback was refused");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn real_no_init_and_rejected_init_end_as_empty_without_unconfirmed_text() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let children = ChildEnds::new();
    let output = children.output();
    let fired = Arc::new(AtomicUsize::new(0));
    let host = EofBeforeRegister {
        inner: RealHost::watching(Arc::new(HostStreams(Arc::clone(&children)))),
        output: Arc::clone(&output),
        fired: Arc::clone(&fired),
    };
    let seams = Seams {
        host,
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(ScriptedRunner::scripted([common::passes()])),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
        resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
        adapters: [Box::new(LocalAdapter) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children,
        witness: Witness::default(),
    };
    let mut running = common::start(boot(&fixture.settings(), seams).expect("boot"));
    let mut agents = Vec::new();
    for label in ["no-init", "owner", "rejected", "overcap"] {
        let dir = fixture.state.path().join(label);
        std::fs::create_dir(&dir).expect("child work directory");
        let agent = common::hire(&running.handle, &dir);
        common::set_goal(
            &running.handle,
            common::goal_params(agent, &["/usr/bin/true"]),
        );
        fixture.await_record("accepted child end", |r| {
            matches!(r, Record::TurnEnded { agent: found, .. } if *found == agent).then_some(())
        });
        agents.push(agent);
    }
    assert_eq!(
        fired.load(Ordering::SeqCst),
        4,
        "all four real children reached EOF before registration"
    );
    let owner_session = fixture.await_record("owner session confirmed", |r| match r {
        Record::SessionLearned { agent, session, .. } if *agent == agents[1] => {
            Some(session.clone())
        }
        _ => None,
    });
    assert_eq!(
        owner_session.as_str(),
        "00000000-0000-4000-8000-000000000099"
    );
    let overcap_session = fixture.await_record("overcap session durably accepted", |r| match r {
        Record::SessionLearned { agent, session, .. } if *agent == agents[3] => {
            Some(session.clone())
        }
        _ => None,
    });
    assert_eq!(overcap_session.as_str().len(), 257);
    let records = fixture.records();
    for agent in [agents[0], agents[2], agents[3]] {
        if agent != agents[3] {
            assert!(
                !records.iter().any(
                    |r| matches!(r, Record::SessionLearned { agent: found, .. } if *found == agent)
                ),
                "no unaccepted session was journaled"
            );
        }
        let ended = await_output(&output, agent, |s| {
            matches!(s, OutputState::Ready { ended: true, .. })
        });
        assert!(
            matches!(ended, OutputState::Ready { ref chunks, first_seq: 0, next_seq: 0, .. } if chunks.is_empty()),
            "unconfirmed or overcap-session text is never visible"
        );
    }
    let owner = await_output(&output, agents[1], |s| {
        matches!(s, OutputState::Ready { ended: true, .. })
    });
    assert_eq!(
        text(&owner),
        "owner-owned-text",
        "accepted init remains the positive control"
    );
    assert!(
        output.usage().refused >= 3,
        "unconfirmed and overcap chunks were counted without content"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn real_durable_overcap_session_acknowledges_empty_live_view_before_end() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let dir = fixture.state.path().join("overcap-live");
    std::fs::create_dir(&dir).expect("held child work directory");
    let children = ChildEnds::new();
    let output = children.output();
    let seams = Seams {
        host: RealHost::watching(Arc::new(HostStreams(Arc::clone(&children)))),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(ScriptedRunner::scripted([common::passes()])),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
        resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
        adapters: [Box::new(LocalAdapter) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children,
        witness: Witness::default(),
    };
    let mut running = common::start(boot(&fixture.settings(), seams).expect("boot"));
    let agent = common::hire(&running.handle, &dir);
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    await_file(&dir.join("started"));
    let before = output.snapshot(agent);
    assert!(matches!(
        before.state,
        OutputState::Unavailable(Unavailable::Unbound)
    ));
    std::fs::write(dir.join("release-init"), b"go").expect("release init");
    let (turn, session) = fixture.await_record("durable overcap SessionLearned", |r| match r {
        Record::SessionLearned {
            agent: found,
            turn,
            session,
        } if *found == agent => Some((*turn, session.clone())),
        _ => None,
    });
    assert_eq!(session.as_str().len(), 257);
    let live = await_output(
        &output,
        agent,
        |s| matches!(s, OutputState::Ready { turn: seen, ended: false, .. } if *seen == turn),
    );
    assert!(
        matches!(live, OutputState::Ready { first_seq: 0, next_seq: 0, ref chunks, .. } if chunks.is_empty())
    );
    assert!(
        output.snapshot(agent).revision > before.revision,
        "accepted confirmation woke the output reader"
    );
    assert_eq!(output.usage().identity_bytes, 0);
    assert_eq!(output.usage().text_bytes, 0);
    assert!(
        !fixture
            .records()
            .iter()
            .any(|r| matches!(r, Record::TurnEnded { agent: found, .. } if *found == agent)),
        "child remains live at the empty acknowledgement"
    );
    std::fs::write(dir.join("release-exit"), b"go").expect("release end");
    fixture.await_record("accepted held-child TurnEnded", |r| matches!(r, Record::TurnEnded { agent: found, turn: seen, .. } if *found == agent && *seen == turn).then_some(()));
    let ended = await_output(
        &output,
        agent,
        |s| matches!(s, OutputState::Ready { turn: seen, ended: true, .. } if *seen == turn),
    );
    assert!(text(&ended).is_empty());
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
// One named, ordered real-child scenario is the acceptance gate; splitting it into
// separate tests would lose the interleaving and replacement-turn assertions.
#[allow(clippy::too_many_lines)]
fn capture_attributed_assistant_text_in_bounded_run_registry() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let children = ChildEnds::new();
    let output = children.output();
    let seams = Seams {
        host: RealHost::watching(Arc::new(HostStreams(Arc::clone(&children)))),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(ScriptedRunner::scripted([common::passes()])),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
        resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
        adapters: [Box::new(LocalAdapter) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children: Arc::clone(&children),
        witness: Witness::default(),
    };
    let daemon = boot(&fixture.settings(), seams).expect("boot");
    let mut running = common::start(daemon);
    let dirs: Vec<PathBuf> = ["A", "B"]
        .iter()
        .map(|name| {
            let path = fixture.state.path().join(name);
            std::fs::create_dir(&path).expect("child work directory");
            path
        })
        .collect();
    let agents: Vec<AgentId> = dirs
        .iter()
        .map(|dir| {
            let agent = common::hire(&running.handle, dir);
            common::set_goal(
                &running.handle,
                common::goal_params(agent, &["/usr/bin/true"]),
            );
            agent
        })
        .collect();
    for dir in &dirs {
        await_file(&dir.join("started"));
    }
    let records = fixture.records();
    let issued: Vec<(AgentId, TurnId)> = records
        .iter()
        .filter_map(|r| match r {
            Record::TurnIssued { agent, turn, .. } if agents.contains(agent) => {
                Some((*agent, *turn))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        issued.len(),
        2,
        "both real owned children reached durable TurnIssued"
    );
    for agent in &agents {
        assert!(
            matches!(
                output.snapshot(*agent).state,
                OutputState::Unavailable(Unavailable::Unbound)
            ),
            "pre-init text is not visible"
        );
    }
    std::fs::write(dirs[1].join("release-init"), b"go").expect("release B init");
    fixture.await_record("B SessionLearned", |r| {
        matches!(r, Record::SessionLearned { agent, .. } if *agent == agents[1]).then_some(())
    });
    let b = await_output(&output, agents[1], |s| text(s).contains("B-live"));
    assert!(
        text(&b).contains("B-early")
            && !text(&b).contains("A-")
            && !text(&b).contains("WRONG-SESSION")
    );
    assert!(matches!(
        output.snapshot(agents[0]).state,
        OutputState::Unavailable(Unavailable::Unbound)
    ));
    std::fs::write(dirs[0].join("release-init"), b"go").expect("release A init");
    fixture.await_record("A SessionLearned", |r| {
        matches!(r, Record::SessionLearned { agent, .. } if *agent == agents[0]).then_some(())
    });
    await_output(&output, agents[0], |s| text(s).contains("A-live"));
    for dir in &dirs {
        std::fs::write(dir.join("release-wrong"), b"go").expect("release post-confirm lines");
    }
    for dir in &dirs {
        await_file(&dir.join("wrote"));
    }
    let b_complete = await_output(&output, agents[1], |s| text(s).contains("B-last"));
    assert_eq!(text(&b_complete), "B-early\nB-live\nB-last");
    assert!(
        !text(&b_complete).contains("WRONG-SESSION")
            && !text(&b_complete).contains("synthetic")
            && !text(&b_complete).contains("OVERSIZE")
    );
    let a = await_output(
        &output,
        agents[0],
        |s| matches!(s, OutputState::Ready { next_seq, .. } if *next_seq >= 134),
    );
    assert!(
        !text(&a).contains("A-live"),
        "truncated prefix is labelled, not fabricated"
    );
    assert!(
        !text(&a).contains("B-")
            && !text(&a).contains("WRONG-SESSION")
            && !text(&a).contains("synthetic")
    );
    assert!(
        matches!(a, OutputState::Ready { first_seq, truncated: true, loss: Some(Loss::RingTruncation), .. } if first_seq > 0)
    );
    let usage = output.usage();
    assert!(usage.chunks <= 128 * 2 && usage.text_bytes <= 65_536 * 2);

    // A changed adoption key with the same pid is not the real child's identity.
    let spawned: Vec<_> = fixture
        .records()
        .into_iter()
        .filter_map(|r| match r {
            Record::TurnSpawned {
                turn,
                pid,
                start_time,
                boot,
                ..
            } if turn == issued[0].1 => Some((pid, start_time, boot)),
            _ => None,
        })
        .collect();
    assert_eq!(spawned.len(), 1);
    let (pid, start_time, boot_id) = spawned[0].clone();
    let changed = AdoptionKey {
        pid,
        start_time: start_time + 1,
        boot: boot_id.clone(),
    };
    assert!(
        !output.register(agents[1], issued[0].1, &changed),
        "a wrong-agent/turn registration is refused"
    );
    let mut stale = children.watch_with(Some(&changed), Box::new(TurnStream::new()));
    stale.line(r#"{"type":"assistant","session_id":"wrong","message":{"role":"assistant","content":[{"type":"text","text":"A-recycled-key"}]}}"#);
    assert!(!text(&output.snapshot(agents[0]).state).contains("recycled-key"));

    for dir in &dirs {
        std::fs::write(dir.join("release-exit"), b"go").expect("release end");
    }
    for agent in &agents {
        fixture.await_record("accepted TurnEnded", |r| {
            matches!(r, Record::TurnEnded { agent: found, .. } if found == agent).then_some(())
        });
    }
    await_output(&output, agents[1], |s| {
        matches!(s, OutputState::Ready { ended: true, .. })
    });

    // Re-use A's agent identity on a new durable turn. The old child key and
    // turn cannot register or append to its replacement ring.
    common::set_goal(
        &running.handle,
        common::goal_params(agents[0], &["/usr/bin/true"]),
    );
    let new_turn = fixture.await_record("A replacement TurnIssued", |r| match r {
        Record::TurnIssued { agent, turn, .. } if *agent == agents[0] && *turn != issued[0].1 => {
            Some(*turn)
        }
        _ => None,
    });
    fixture.await_record("A replacement TurnEnded", |r| {
        matches!(r, Record::TurnEnded { turn, .. } if *turn == new_turn).then_some(())
    });
    assert!(
        !output.register(agents[0], issued[0].1, &changed),
        "old turn cannot claim new ring"
    );
    let turn_probe = OutputRegistry::new();
    turn_probe.replace(agents[0], new_turn);
    assert!(
        !turn_probe.register(agents[0], issued[0].1, &changed),
        "an old durable TurnId cannot claim a prepared replacement, even before its child registers"
    );
    let mut late = children.watch_with(
        Some(&AdoptionKey {
            pid,
            start_time,
            boot: boot_id,
        }),
        Box::new(TurnStream::new()),
    );
    late.line(r#"{"type":"assistant","session_id":"wrong","message":{"role":"assistant","content":[{"type":"text","text":"late-old-turn"}]}}"#);
    let replacement = await_output(
        &output,
        agents[0],
        |s| matches!(s, OutputState::Ready { turn, ended: true, .. } if *turn == new_turn),
    );
    assert!(
        matches!(&replacement, OutputState::Ready { turn, ended: true, .. } if *turn == new_turn)
    );
    assert!(!text(&replacement).contains("late-old-turn"));

    // More than 8 MiB admitted from actual process stdout across 129+ rings.
    for n in 0..129 {
        let dir = fixture.state.path().join(format!("bulk-{n}"));
        std::fs::create_dir(&dir).expect("bulk work directory");
        let agent = common::hire(&running.handle, &dir);
        common::set_goal(
            &running.handle,
            common::goal_params(agent, &["/usr/bin/true"]),
        );
        fixture.await_record("bulk TurnEnded", |r| {
            matches!(r, Record::TurnEnded { agent: found, .. } if *found == agent).then_some(())
        });
        let bulk = await_output(
            &output,
            agent,
            |s| matches!(s, OutputState::Ready { ended: true, next_seq: 16, chunks, .. } if chunks.len() == 16),
        );
        assert!(
            matches!(bulk, OutputState::Ready { ref chunks, .. } if chunks.iter().map(|c| c.text.len()).sum::<usize>() == 65_536)
        );
        let usage = output.usage();
        assert!(
            usage.rings <= 128 && usage.text_bytes <= 8 * 1_024 * 1_024,
            "run-local bounds"
        );
    }
    assert!(matches!(
        output.snapshot(agents[0]).state,
        OutputState::Unavailable(Unavailable::Evicted)
    ));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    let restarted_children = ChildEnds::new();
    let restarted_output = restarted_children.output();
    let restarted = boot(
        &fixture.settings(),
        Seams {
            host: RealHost::watching(Arc::new(HostStreams(Arc::clone(&restarted_children)))),
            pty_host: Box::new(duded::ports::ClosedPtyHost),
            predicates: Arc::new(ScriptedRunner::scripted([common::passes()])),
            clock: Arc::new(TestClock::new()),
            rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
            resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
            adapters: [Box::new(LocalAdapter) as Box<dyn Adapter>]
                .into_iter()
                .collect(),
            children: restarted_children,
            witness: Witness::default(),
        },
    )
    .expect("replay and restart");
    assert!(
        matches!(
            restarted_output.snapshot(agents[1]).state,
            OutputState::Unavailable(Unavailable::NoRunOutput)
        ),
        "replayed journal state does not reconstruct transient output"
    );
    assert!(
        fixture
            .records()
            .iter()
            .any(|r| matches!(r, Record::TurnEnded { agent, .. } if *agent == agents[1]))
    );
    let mut restarted = common::start(restarted);
    assert_eq!(
        restarted.stop(ExitCause::ShutdownRpc),
        ExitCause::ShutdownRpc
    );
}
