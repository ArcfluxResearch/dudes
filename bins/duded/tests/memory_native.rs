//! Memory RPC integration tests.
#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use std::ffi::{OsStr, OsString};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::classify::{ExternalChild, PeerBarrier, ProcessRelation, Relation, Resolver};
use duded::ports::{Answered, Ask, PtyChild};
use duded::startup::Witness;
use duded::{ChildEnds, ChildTurn, ExitCause};
use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_proto::rpc::memory::{MemoryAppendResult, MemoryReadResult};
use dudes_proto::rpc::{ErrorCode, Method};
use dudes_proto::{ExitStatus, Record};
use dudes_providers::adapter::{
    Adapter, AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
};
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_providers::stream::TurnReader;
use dudes_supervisor::host::{
    AdoptionKey, HostBoot, KillOutcome, Liveness, NativeBoundaryAcknowledgement,
    NativeBoundaryReceipt, OwnedTargets, ProcessHost, SpawnResult,
};
use dudes_supervisor::testing::FakeRng;
use dudes_types::{AgentId, Lsn, ModelSelection, Provider, Tier};
use serde_json::json;

const WAIT: Duration = Duration::from_secs(10);
const ROLE: &str = "DUDES_MEMORY_HELPER";

struct Streams(Arc<ChildEnds>);
struct Stream(ChildTurn);
impl ChildStreams for Streams {
    fn watch(&self, key: Option<&AdoptionKey>) -> Box<dyn ChildStream> {
        Box::new(Stream(self.0.watch(key)))
    }
}
impl ChildStream for Stream {
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

#[derive(Debug)]
struct Processes(dudes_host::PeerProcesses);
#[derive(Debug)]
struct External(dudes_host::ExternalChild);
impl ExternalChild for External {
    fn register(&mut self, child: PtyChild) -> io::Result<AdoptionKey> {
        self.0.register(child.pid)
    }
}
impl ProcessRelation for Processes {
    fn begin_spawn(&self) -> u64 {
        self.0.begin_spawn()
    }
    fn finish_spawn(&self, slot: u64, key: Option<&AdoptionKey>) -> bool {
        self.0.finish_spawn(slot, key)
    }
    fn published(&self, children: &[AdoptionKey]) -> Vec<(u64, usize)> {
        self.0.published(children)
    }
    fn resolve(&self, peer: u32, pidversion: Option<u32>, children: &[AdoptionKey]) -> Relation {
        #[cfg(target_os = "macos")]
        let answer = pidversion.map_or(dudes_host::PeerRelation::Unresolvable, |version| {
            self.0.resolve_execution(peer, version, children)
        });
        #[cfg(target_os = "linux")]
        let answer = {
            let _ = pidversion;
            self.0.resolve(peer, children)
        };
        match answer {
            dudes_host::PeerRelation::Matched(index) => Relation::Matched(index),
            dudes_host::PeerRelation::Pending(slot) => Relation::Pending(slot),
            dudes_host::PeerRelation::NoMatch => Relation::NoMatch,
            dudes_host::PeerRelation::Unresolvable => Relation::Unresolvable,
            dudes_host::PeerRelation::Abandoned => Relation::Abandoned,
        }
    }
    fn track_external(&self, slot: u64) -> Box<dyn ExternalChild> {
        Box::new(External(self.0.track_external(slot)))
    }
}

struct RetainedHost(RealHost);
impl ProcessHost for RetainedHost {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        self.0.spawn(spec, install)
    }
    fn kill(&mut self, _: &AdoptionKey) -> KillOutcome {
        KillOutcome::Failed(io::Error::other(
            "retained native child for retirement check",
        ))
    }
    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.0.alive(key)
    }
    fn owned_targets(&self, limit: usize) -> OwnedTargets {
        self.0.owned_targets(limit)
    }
    fn acknowledge_native_boundary_absence(
        &mut self,
        key: &AdoptionKey,
        receipt: &NativeBoundaryReceipt,
    ) -> NativeBoundaryAcknowledgement {
        self.0.acknowledge_native_boundary_absence(key, receipt)
    }
    fn boot_identity(&self) -> HostBoot {
        self.0.boot_identity()
    }
    fn shutdown(&mut self) -> io::Result<()> {
        self.0.shutdown()
    }
}

#[derive(Debug)]
struct Helper;
impl Adapter for Helper {
    fn provider(&self) -> Provider {
        Provider::Claude
    }
    fn program(&self) -> &OsStr {
        OsStr::new("claude")
    }
    fn tier(&self) -> Tier {
        Tier::HookGoverned
    }
    fn isolation(&self) -> Isolation {
        Isolation::None
    }
    fn session_acquisition(&self) -> SessionAcquisition {
        dudes_providers::claude::SESSION_ACQUISITION
    }
    fn turn_reader(&self) -> Box<dyn TurnReader> {
        Box::new(dudes_providers::claude::stream::TurnStream::new())
    }
    fn model_selection(&self) -> ModelSelection {
        ModelSelection::provider_default()
    }
    fn managed(
        &self,
        ctx: &AgentCtx<'_>,
        task: &str,
        _: &TurnSession,
        _: &ModelSelection,
    ) -> Result<LaunchSpec, dudes_providers::launch::LaunchError> {
        if task.is_empty() {
            return Err(dudes_providers::launch::LaunchError::EmptyTask);
        }
        if let Some(offset) = task.find('\0') {
            return Err(dudes_providers::launch::LaunchError::TaskContainsNul { offset });
        }
        let mut env = ctx.daemon_env.to_vec();
        env.extend([
            EnvVar::new(ROLE, ctx.cwd.file_name().unwrap()),
            EnvVar::new("DUDES_MEMORY_AGENT", ctx.agent.to_string()),
        ]);
        Ok(LaunchSpec {
            argv: [
                std::env::current_exe().unwrap().into_os_string(),
                OsString::from("--exact"),
                OsString::from("memory_helper"),
                OsString::from("--quiet"),
            ]
            .to_vec(),
            env,
            cwd: ctx.cwd.to_path_buf(),
            stdin: dudes_providers::launch::LaunchInput::WriteThenClose(task.as_bytes().to_vec()),
        })
    }
    fn managed_install(&self, _: &AgentCtx<'_>, _: &ModelSelection) -> ManagedInstall {
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

fn await_file(path: &Path) {
    let deadline = Instant::now() + WAIT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "missing marker {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn exercise_own_memory(
    client: &mut dudes_rpc::LocalClient,
    token: &str,
    text: &str,
    selected: AgentId,
) {
    let call = |client: &mut dudes_rpc::LocalClient, method, params| {
        client.call(method, Some(params)).unwrap()
    };
    let response = call(
        client,
        Method::MemoryAppend,
        json!({"token":token,"text":text}),
    );
    assert!(response.error.is_none(), "append: {response:?}");
    let appended: MemoryAppendResult = serde_json::from_value(response.result.unwrap()).unwrap();
    let response = call(client, Method::MemoryRead, json!({"token":token}));
    let read: MemoryReadResult = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!((read.text, read.lsn), (text.to_owned(), appended.lsn));
    let response = call(
        client,
        Method::MemoryRead,
        json!({"token":token,"since_lsn":appended.lsn}),
    );
    let read: MemoryReadResult = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!((read.text, read.lsn), (String::new(), Lsn(0)));
    assert_eq!(
        call(client, Method::MemoryRead, json!({}))
            .error
            .unwrap()
            .code,
        ErrorCode::Unauthorized.code()
    );
    assert_eq!(
        call(client, Method::MemoryAppend, json!({"text":"no token"}))
            .error
            .unwrap()
            .code,
        ErrorCode::Unauthorized.code()
    );
    let response = call(
        client,
        Method::MemoryRead,
        json!({"token":token,"agent":selected}),
    );
    let read: MemoryReadResult = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(read.text, text);
    assert_eq!(
        call(
            client,
            Method::MemoryAppend,
            json!({"token":token,"agent":selected,"text":"wrong target"})
        )
        .error
        .unwrap()
        .code,
        ErrorCode::InvalidParams.code()
    );
}

fn condense_body(index: usize) -> String {
    let prefix = format!("Decision {index}: use typed argv\n");
    let tail = "\nCause: paths contain spaces";
    format!(
        "{prefix}{}{tail}",
        "x".repeat(2_048 - prefix.len() - tail.len())
    )
}

fn exercise_condense_memory(
    work: &Path,
    role: &str,
    task: &str,
    client: &mut dudes_rpc::LocalClient,
    token: &str,
) {
    let call = |client: &mut dudes_rpc::LocalClient, method, params| {
        client.call(method, Some(params)).unwrap()
    };
    if work.join("first").exists() {
        if role == "condense" {
            assert!(task.contains("[Condensed memory: exact excerpts;"));
            assert!(task.contains("Decision"));
            assert!(task.contains("NEWEST"));
        }
        assert!(task.len() <= 65_536);
        std::fs::write(work.join("carried"), b"").unwrap();
    } else {
        for index in 0..40 {
            let response = call(
                client,
                Method::MemoryAppend,
                json!({"token":token,"text":condense_body(index)}),
            );
            assert!(response.error.is_none());
        }
        assert!(
            call(
                client,
                Method::MemoryAppend,
                json!({"token":token,"text":"NEWEST"})
            )
            .error
            .is_none()
        );
        std::fs::write(work.join("first"), b"").unwrap();
    }
}

#[test]
fn memory_helper() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let work = std::env::current_dir().unwrap();
    let token = std::env::var("DUDES_TOKEN").unwrap();
    let socket = PathBuf::from(std::env::var_os("DUDES_SOCK").unwrap());
    let mut task = String::new();
    std::io::stdin().read_to_string(&mut task).unwrap();
    assert_eq!(
        task.matches("[Memory: read with `dudes memory read`")
            .count(),
        1
    );
    std::fs::write(work.join("task"), &task).unwrap();
    let mut client = dudes_rpc::Client::connect(&socket).unwrap();
    let call = |client: &mut dudes_rpc::LocalClient, method, params| {
        client.call(method, Some(params)).unwrap()
    };
    let text = format!("{role} remembers 🦀\nexactly");
    if role == "condense" || role == "probe" {
        exercise_condense_memory(&work, &role, &task, &mut client, &token);
    } else if role == "again" && work.join("first").exists() {
        assert!(task.contains("again remembers 🦀\nexactly"));
        std::fs::write(work.join("carried"), b"").unwrap();
    } else {
        let selected = if role == "beta" {
            std::fs::read_to_string(work.parent().unwrap().join("alpha/owner"))
                .unwrap()
                .parse()
                .unwrap()
        } else {
            AgentId::NIL
        };
        exercise_own_memory(&mut client, &token, &text, selected);
        if role == "beta" {
            assert_sibling_token_refused(&work, &mut client);
        }
        std::fs::write(
            work.join("owner"),
            std::env::var("DUDES_MEMORY_AGENT").unwrap(),
        )
        .unwrap();
        std::fs::write(work.join("token"), &token).unwrap();
        std::fs::write(work.join("first"), b"").unwrap();
        if role == "alpha" {
            await_file(&work.join("retired"));
            for method in [
                Method::MemoryRead,
                Method::MemoryAppend,
                Method::MemorySearch,
            ] {
                assert_eq!(
                    call(
                        &mut client,
                        method,
                        if method == Method::MemoryRead {
                            json!({"token":token})
                        } else if method == Method::MemorySearch {
                            json!({"token":token,"query":"remembers"})
                        } else {
                            json!({"token":token,"text":"after retirement"})
                        }
                    )
                    .error
                    .unwrap()
                    .code,
                    ErrorCode::Refused.code()
                );
            }
            std::fs::write(work.join("refused-retired"), b"").unwrap();
        }
    }
    let mut out = std::io::stdout().lock();
    writeln!(out, r#"{{"type":"system","subtype":"init","session_id":"3f9a1c2e-0000-4000-8000-0000000012ab","tools":[]}}"#).unwrap();
    writeln!(out, r#"{{"type":"result","subtype":"success","is_error":false,"session_id":"3f9a1c2e-0000-4000-8000-0000000012ab","result":"complete","total_cost_usd":0,"modelUsage":{{}}}}"#).unwrap();
    out.flush().unwrap();
    std::process::exit(0);
}

fn assert_sibling_token_refused(work: &Path, client: &mut dudes_rpc::LocalClient) {
    let sibling = std::fs::read_to_string(work.parent().unwrap().join("alpha/token")).unwrap();
    for method in [
        Method::MemoryRead,
        Method::MemoryAppend,
        Method::MemorySearch,
    ] {
        let params = match method {
            Method::MemoryRead => json!({"token":sibling}),
            Method::MemorySearch => json!({"token":sibling,"query":"remembers"}),
            _ => json!({"token":sibling,"text":"stolen"}),
        };
        let response = client.call(method, Some(params)).unwrap();
        assert_eq!(response.error.unwrap().code, ErrorCode::Unauthorized.code());
    }
}

struct Native {
    running: common::Running,
    pause: duded::runtime::OnboardingPause,
    failure: duded::runtime::AppendFailure,
}

fn native(
    fixture: &common::Fixture,
    predicates: Vec<duded::predicate::PredicateOutcome>,
) -> Native {
    native_with_memory_config(
        fixture,
        predicates,
        dudes_journal::MemoryCondenseConfig::default(),
    )
}

fn native_with_memory_config(
    fixture: &common::Fixture,
    predicates: Vec<duded::predicate::PredicateOutcome>,
    memory: dudes_journal::MemoryCondenseConfig,
) -> Native {
    native_with_rng(fixture, predicates, memory, FakeRng::new())
}

fn native_with_rng(
    fixture: &common::Fixture,
    predicates: Vec<duded::predicate::PredicateOutcome>,
    memory: dudes_journal::MemoryCondenseConfig,
    rng: FakeRng,
) -> Native {
    let settings = fixture.settings();
    let children = ChildEnds::new();
    let host = RealHost::watching(Arc::new(Streams(Arc::clone(&children))));
    let resolver = Resolver::processes(Arc::new(Processes(host.peer_processes())));
    let seams = duded::settings::Seams {
        host: PeerBarrier::new(RetainedHost(host), resolver.clone()),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(common::ScriptedRunner::scripted(predicates)),
        clock: Arc::new(common::TestClock::new()),
        rng,
        resolver,
        adapters: [Box::new(Helper) as Box<dyn Adapter>].into_iter().collect(),
        children,
        witness: Witness::default(),
    };
    let daemon = duded::boot::boot_with_memory_config(&settings, seams, memory).unwrap();
    let pause = daemon.onboarding_pause();
    let failure = daemon.append_failure();
    Native {
        running: common::start(daemon),
        pause,
        failure,
    }
}

#[test]
fn native_overbound_memory_condense_reaches_the_child_and_replays_raw_pages() {
    let fixture = common::Fixture::new();
    let Native { mut running, .. } = native(&fixture, vec![common::fails(), common::passes()]);
    let work = fixture.state.path().join("condense");
    let agent = hire_at(&running, &work);
    await_file(&work.join("carried"));
    let task = std::fs::read_to_string(work.join("task")).unwrap();
    assert!(task.contains("[Condensed memory: exact excerpts;"));
    assert!(task.contains("NEWEST"));
    assert!(task.len() <= 65_536);
    let mut cursor = None;
    let mut pages = Vec::new();
    loop {
        let Answered::MemoryRead(page) = running
            .handle
            .ask(Ask::MemoryRead {
                agent,
                caller: None,
                since_lsn: cursor,
            })
            .unwrap()
        else {
            panic!("page")
        };
        if page.lsn == Lsn::ZERO {
            break;
        }
        cursor = Some(page.lsn);
        pages.push(page);
    }
    assert_eq!(
        pages
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        (0..40)
            .map(condense_body)
            .chain(std::iter::once("NEWEST".to_owned()))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(fixture.records().iter().any(
        |record| matches!(record, Record::MemoryCondense { agent: who, .. } if *who == agent)
    ));
    fixture.await_record("completed memory goal", |record| match record {
        Record::LoopDecided {
            agent: who,
            decision: dudes_proto::LoopDecision::Done,
            ..
        } if *who == agent => Some(()),
        _ => None,
    });
    running.stop(ExitCause::ShutdownRpc);
    let mut replay = dudes_journal::MemoryProjection::new(fixture.state.path()).unwrap();
    for (lsn, entry) in fixture.entries() {
        replay.apply_record(lsn, &entry.record).unwrap();
    }
    assert!(
        replay
            .work_digest(agent)
            .contains("[Condensed memory: exact excerpts;")
    );
    assert!(replay.work_digest(agent).contains("NEWEST"));
    let mut cursor = None;
    for page in &pages {
        assert_eq!(
            replay.read(agent, cursor, 65_536).unwrap(),
            (page.text.clone(), page.lsn)
        );
        cursor = Some(page.lsn);
    }
    assert_eq!(
        replay.read(agent, cursor, 65_536).unwrap(),
        (String::new(), Lsn::ZERO)
    );
    save_condense_receipts(&fixture, agent, &task, &replay, &pages);
    assert_restarted_memory_launch(&fixture, agent, &work);
}

fn assert_restarted_memory_launch(fixture: &common::Fixture, agent: AgentId, work: &Path) {
    std::fs::remove_file(work.join("carried")).unwrap();
    let mut rng = FakeRng::new();
    for _ in fixture.entries() {
        dudes_supervisor::host::Rng::ulid(&mut rng);
    }
    let Native {
        running: mut restarted,
        ..
    } = native_with_rng(
        fixture,
        vec![common::passes()],
        dudes_journal::MemoryCondenseConfig::default(),
        rng,
    );
    let goal = common::set_goal(
        &restarted.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let issued = fixture.await_record("restarted memory launch", |record| match record {
        Record::TurnIssued {
            goal: issued_goal,
            task,
            ..
        } if *issued_goal == goal => Some(task.clone()),
        _ => None,
    });
    assert!(
        issued.contains("[Condensed memory: exact excerpts;"),
        "boot replay must retain the durable summary in the next issued task"
    );
    assert!(issued.contains("NEWEST"));
    await_file(&work.join("carried"));
    assert_eq!(std::fs::read_to_string(work.join("task")).unwrap(), issued);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::MemoryCondense { .. }))
            .count(),
        1,
        "restart must use the durable summary without a second condense"
    );
    restarted.stop(ExitCause::ShutdownRpc);
}

fn save_condense_receipts(
    fixture: &common::Fixture,
    agent: AgentId,
    task: &str,
    replay: &dudes_journal::MemoryProjection,
    pages: &[MemoryReadResult],
) {
    if let Some(output) = std::env::var_os("DUDES_MEMORY_CONDENSE_RECEIPTS") {
        let output = PathBuf::from(output);
        std::fs::create_dir_all(&output).unwrap();
        std::fs::write(output.join("received-task.txt"), task).unwrap();
        std::fs::write(
            output.join("memory-export.json"),
            replay.export(agent).unwrap(),
        )
        .unwrap();
        std::fs::write(
            output.join("journal-export.json"),
            serde_json::to_vec_pretty(&fixture.entries()).unwrap(),
        )
        .unwrap();
        let raw = pages
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let summaries = fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::MemoryCondense { .. }))
            .count();
        let metrics = json!({"raw_bytes":raw.len(),"active_bytes":replay.active_bytes(agent),"work_bytes":replay.work_digest(agent).len(),"task_bytes":task.len(),"condense_records":summaries,"newest_retained":task.contains("NEWEST"),"summary_retained":task.contains("[Condensed memory: exact excerpts;")});
        std::fs::write(
            output.join("metrics.json"),
            serde_json::to_vec_pretty(&metrics).unwrap(),
        )
        .unwrap();
        for entry in std::fs::read_dir(fixture.journal_dir()).unwrap() {
            let path = entry.unwrap().path();
            let destination = output.join("journal");
            std::fs::create_dir_all(&destination).unwrap();
            std::fs::copy(&path, destination.join(path.file_name().unwrap())).unwrap();
        }
        for record in fixture.records() {
            if let Record::MemoryCondense { summary_ref, .. } = record {
                let target = output.join(&summary_ref);
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::copy(fixture.state.path().join(summary_ref), target).unwrap();
            }
        }
    }
}

#[test]
fn native_memory_task_size_probe() {
    let started = Instant::now();
    let fixture = common::Fixture::new();
    let Native { mut running, .. } = native_with_memory_config(
        &fixture,
        vec![common::fails(), common::passes()],
        dudes_journal::MemoryCondenseConfig::new(1_024, 8).unwrap(),
    );
    let work = fixture.state.path().join("probe");
    hire_at(&running, &work);
    await_file(&work.join("carried"));
    let task = std::fs::read_to_string(work.join("task")).unwrap();
    let baseline = std::env::var_os("DUDES_MEMORY_PROBE_EXPECT_BASELINE").is_some();
    assert_eq!(task.contains("NEWEST"), !baseline);
    assert!(task.len() <= 65_536);
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::MemoryCondense { .. }))
    );
    let raw = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::MemoryAppend {
                text, body: None, ..
            } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(raw.len(), 81_966);
    running.stop(ExitCause::ShutdownRpc);
    let metrics = json!({"task_bytes":task.len(),"newest_retained":task.contains("NEWEST"),"raw_bytes":raw.len(),"baseline":baseline,"elapsed_ms":started.elapsed().as_millis()});
    if let Some(path) = std::env::var_os("DUDES_MEMORY_PROBE_RECEIPT") {
        std::fs::write(path, serde_json::to_vec_pretty(&metrics).unwrap()).unwrap();
    }
}

#[test]
fn restored_backlog_condenses_once_on_tick_without_an_extra_turn() {
    let fixture = common::Fixture::new();
    let Native { mut running, .. } = native_with_memory_config(
        &fixture,
        vec![common::fails(), common::passes()],
        dudes_journal::MemoryCondenseConfig::new(1_024, 8).unwrap(),
    );
    let work = fixture.state.path().join("probe");
    let agent = hire_at(&running, &work);
    await_file(&work.join("carried"));
    fixture.await_record("observed done", |record| match record {
        Record::LoopDecided {
            decision: dudes_proto::LoopDecision::Done,
            ..
        } => Some(()),
        _ => None,
    });
    running.stop(ExitCause::ShutdownRpc);
    let before = fixture
        .records()
        .iter()
        .filter(|r| matches!(r, Record::TurnIssued { .. }))
        .count();
    let Native {
        running: mut restarted,
        ..
    } = native(&fixture, vec![]);
    restarted.handle.input(dudes_supervisor::Input::Tick);
    fixture.await_record("catch-up condense", |record| match record {
        Record::MemoryCondense { agent: who, .. } if *who == agent => Some(()),
        _ => None,
    });
    restarted.handle.input(dudes_supervisor::Input::Tick);
    let Answered::MemoryRead(page) = restarted
        .handle
        .ask(Ask::MemoryRead {
            agent,
            caller: None,
            since_lsn: None,
        })
        .unwrap()
    else {
        panic!("page")
    };
    assert!(page.text.starts_with("Decision 0:"));
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|r| matches!(r, Record::MemoryCondense { .. }))
            .count(),
        1
    );
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|r| matches!(r, Record::TurnIssued { .. }))
            .count(),
        before
    );
    restarted.stop(ExitCause::ShutdownRpc);
}

struct ReleaseMemoryPause(duded::runtime::MemoryCondensePause);
impl Drop for ReleaseMemoryPause {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[test]
fn a_detected_durable_summary_invalidation_fences_later_local_work() {
    let fixture = common::Fixture::new();
    let settings = fixture.settings();
    let children = ChildEnds::new();
    let host = RealHost::watching(Arc::new(Streams(Arc::clone(&children))));
    let resolver = Resolver::processes(Arc::new(Processes(host.peer_processes())));
    let seams = duded::settings::Seams {
        host: PeerBarrier::new(RetainedHost(host), resolver.clone()),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(common::ScriptedRunner::scripted([common::passes()])),
        clock: Arc::new(common::TestClock::new()),
        rng: FakeRng::new(),
        resolver,
        adapters: [Box::new(Helper) as Box<dyn Adapter>].into_iter().collect(),
        children,
        witness: Witness::default(),
    };
    let daemon = duded::boot::boot_with_memory_config(
        &settings,
        seams,
        dudes_journal::MemoryCondenseConfig::new(1, 1).unwrap(),
    )
    .unwrap();
    let pause = daemon.memory_condense_pause();
    let release = ReleaseMemoryPause(pause.clone());
    let mut running = common::start(daemon);
    let work = fixture.state.path().join("fenced");
    std::fs::create_dir(&work).unwrap();
    let agent = common::hire(&running.handle, &work);
    running
        .handle
        .ask(Ask::MemoryAppend {
            agent,
            payload: condense_body(0).into_bytes(),
            provenance: dudes_proto::PeerProvenance::Agent(agent),
        })
        .unwrap();
    pause.arm();
    let handle = running.handle.clone();
    let appending = std::thread::spawn(move || {
        handle.ask(Ask::MemoryAppend {
            agent,
            payload: condense_body(1).into_bytes(),
            provenance: dudes_proto::PeerProvenance::Agent(agent),
        })
    });
    assert!(pause.wait_reached(WAIT));
    let path = fixture
        .records()
        .iter()
        .find_map(|record| match record {
            Record::MemoryCondense { summary_ref, .. } => {
                Some(fixture.state.path().join(summary_ref))
            }
            _ => None,
        })
        .unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 1;
    std::fs::write(&path, &bytes).unwrap();
    drop(release);
    assert!(appending.join().unwrap().is_err());
    let before = fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::TurnIssued { .. }))
        .count();
    assert!(
        running
            .handle
            .ask(Ask::SetGoal(Box::new(common::goal_params(
                agent,
                &["/usr/bin/true"]
            ))))
            .is_err(),
        "local work must be refused after durable memory integrity failure"
    );
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::TurnIssued { .. }))
            .count(),
        before
    );
    assert_eq!(before, 0);
    running.stop(ExitCause::ShutdownRpc);
}

fn hire_at(running: &common::Running, path: &Path) -> AgentId {
    std::fs::create_dir(path).unwrap();
    let agent = common::hire(&running.handle, path);
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    agent
}

#[test]
fn native_memory_lineage_retirement_and_operator_read() {
    let fixture = common::Fixture::new();
    let Native { mut running, .. } = native(&fixture, vec![common::passes(), common::passes()]);
    let alpha = fixture.state.path().join("alpha");
    let a = hire_at(&running, &alpha);
    await_file(&alpha.join("first"));
    let beta = fixture.state.path().join("beta");
    let b = hire_at(&running, &beta);
    await_file(&beta.join("first"));
    let mut operator = dudes_rpc::Client::connect(fixture.settings().socket()).unwrap();
    let credential = || {
        dudes_proto::rpc::cred::OperatorCredential::new(
            std::fs::read_to_string(
                fixture
                    .settings()
                    .runtime_dir
                    .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
            )
            .unwrap(),
        )
        .unwrap()
    };
    for (agent, role) in [(a, "alpha"), (b, "beta")] {
        let response = operator
            .call_as_operator(
                Method::MemoryRead,
                Some(json!({"agent":agent})),
                credential(),
            )
            .unwrap();
        let result: MemoryReadResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(result.text, format!("{role} remembers 🦀\nexactly"));
    }
    running
        .handle
        .ask(Ask::Retire(Box::new(
            dudes_proto::rpc::agent::RetireParams {
                agent: a,
                reason: "memory test".to_owned(),
            },
        )))
        .unwrap();
    let refused = running
        .handle
        .ask(Ask::MemoryRead {
            agent: a,
            caller: Some(a),
            since_lsn: None,
        })
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::Refused.code());
    std::fs::write(alpha.join("retired"), b"").unwrap();
    await_file(&alpha.join("refused-retired"));
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::MemoryAppend { .. }))
            .count(),
        2
    );
    let Answered::MemoryRead(operator) = running
        .handle
        .ask(Ask::MemoryRead {
            agent: a,
            caller: None,
            since_lsn: None,
        })
        .unwrap()
    else {
        panic!("memory result")
    };
    assert_eq!(operator.text, "alpha remembers 🦀\nexactly");
    running.stop(ExitCause::ShutdownRpc);
}

#[test]
fn native_memory_reaches_the_next_launch_and_replays() {
    let fixture = common::Fixture::new();
    let Native { mut running, .. } = native(&fixture, vec![common::fails(), common::passes()]);
    let work = fixture.state.path().join("again");
    let agent = hire_at(&running, &work);
    await_file(&work.join("carried"));
    let live = running
        .handle
        .ask(Ask::MemoryRead {
            agent,
            caller: None,
            since_lsn: None,
        })
        .unwrap();
    running.stop(ExitCause::ShutdownRpc);
    let mut replay = dudes_journal::MemoryProjection::new(fixture.state.path()).unwrap();
    for (lsn, entry) in fixture.entries() {
        replay.apply(lsn, &entry.record);
    }
    let Answered::MemoryRead(live) = live else {
        panic!("memory result")
    };
    assert_eq!(
        replay.read(agent, None, 65_536),
        Ok((live.text.clone(), live.lsn))
    );
    let Native {
        running: mut restarted,
        ..
    } = native(&fixture, vec![]);
    let Answered::MemoryRead(after) = restarted
        .handle
        .ask(Ask::MemoryRead {
            agent,
            caller: None,
            since_lsn: None,
        })
        .unwrap()
    else {
        panic!("replayed read")
    };
    assert_eq!(after, live);
    restarted.stop(ExitCause::ShutdownRpc);
}

#[test]
fn memory_reads_queued_before_retirement_use_the_fold_and_survive_writer_poison() {
    let fixture = common::Fixture::new();
    let Native {
        mut running,
        pause,
        failure,
    } = native(&fixture, vec![common::passes()]);
    let work = fixture.state.path().join("alpha");
    let agent = hire_at(&running, &work);
    await_file(&work.join("first"));
    let appended = fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| matches!(entry.record, Record::MemoryAppend { .. }).then_some(lsn))
        .unwrap();
    pause.arm(duded::runtime::OnboardingPausePoint::BeforeRetirePermit);
    let retirement = running
        .handle
        .ask_queued(Ask::Retire(Box::new(
            dudes_proto::rpc::agent::RetireParams {
                agent,
                reason: "queued read".into(),
            },
        )))
        .unwrap();
    assert!(pause.wait_reached(WAIT));
    assert!(
        fixture
            .records()
            .iter()
            .all(|record| !matches!(record, Record::AgentRetired { .. }))
    );
    let read = running
        .handle
        .ask_queued(Ask::MemoryRead {
            agent,
            caller: Some(agent),
            since_lsn: None,
        })
        .unwrap();
    assert!(matches!(
        read.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    pause.release();
    retirement.recv_timeout(WAIT).unwrap().unwrap();
    let refused = read.recv_timeout(WAIT).unwrap().unwrap_err();
    assert_eq!(refused.code, -32004);
    assert_eq!(refused.data, Some(json!({"gate":"token"})));
    failure.short_write_next(5);
    assert!(
        running
            .handle
            .ask(Ask::Hire(Box::new(common::hire_params(
                fixture.state.path()
            ))))
            .is_err()
    );
    let refused = running
        .handle
        .ask(Ask::MemoryRead {
            agent,
            caller: Some(agent),
            since_lsn: None,
        })
        .unwrap_err();
    assert_eq!(refused.code, -32004);
    assert_eq!(refused.data, Some(json!({"gate":"token"})));
    let Answered::MemoryRead(read) = running
        .handle
        .ask(Ask::MemoryRead {
            agent,
            caller: None,
            since_lsn: None,
        })
        .unwrap()
    else {
        panic!("operator read")
    };
    assert_eq!(read.text, "alpha remembers 🦀\nexactly");
    assert_eq!(read.lsn, appended);
    std::fs::write(work.join("retired"), b"").unwrap();
    await_file(&work.join("refused-retired"));
    running.stop(ExitCause::ShutdownRpc);
}

#[test]
fn memory_searches_queued_before_retirement_refuse_current_and_parked_callers() {
    use dudes_proto::rpc::memory::{MemorySearchHit, MemorySearchOptions};
    let fixture = common::Fixture::new();
    let Native {
        mut running,
        pause,
        failure,
    } = native(&fixture, vec![common::passes()]);
    let work = fixture.state.path().join("alpha");
    let agent = hire_at(&running, &work);
    await_file(&work.join("first"));
    pause.arm(duded::runtime::OnboardingPausePoint::BeforeRetirePermit);
    let retirement = running
        .handle
        .ask_queued(Ask::Retire(Box::new(
            dudes_proto::rpc::agent::RetireParams {
                agent,
                reason: "queued search".into(),
            },
        )))
        .unwrap();
    assert!(pause.wait_reached(WAIT));
    let options = || MemorySearchOptions::new("remembers".into(), None, None).unwrap();
    let queued = running
        .handle
        .ask_queued(Ask::MemorySearch {
            agent,
            caller: Some(agent),
            options: options(),
        })
        .unwrap();
    assert!(matches!(
        queued.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    pause.release();
    retirement.recv_timeout(WAIT).unwrap().unwrap();
    let refused = queued.recv_timeout(WAIT).unwrap().unwrap_err();
    assert_eq!(refused.code, -32004);
    assert_eq!(refused.data, Some(json!({"gate":"token"})));
    failure.short_write_next(5);
    assert!(
        running
            .handle
            .ask(Ask::Hire(Box::new(common::hire_params(
                fixture.state.path()
            ))))
            .is_err()
    );
    let refused = running
        .handle
        .ask(Ask::MemorySearch {
            agent,
            caller: Some(agent),
            options: options(),
        })
        .unwrap_err();
    assert_eq!(refused.code, -32004);
    assert_eq!(refused.data, Some(json!({"gate":"token"})));
    let Answered::MemorySearch(page) = running
        .handle
        .ask(Ask::MemorySearch {
            agent,
            caller: None,
            options: options(),
        })
        .unwrap()
    else {
        panic!("operator search")
    };
    assert_eq!(page.hits.len(), 1);
    assert!(
        matches!(&page.hits[0],MemorySearchHit::Raw{text,..} if text=="alpha remembers 🦀\nexactly")
    );
    std::fs::write(work.join("retired"), b"").unwrap();
    await_file(&work.join("refused-retired"));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}
