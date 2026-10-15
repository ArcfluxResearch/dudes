#![expect(
    missing_docs,
    reason = "Integration tests do not expose a library API."
)]
#![cfg(any(target_os = "macos", target_os = "linux"))]

mod common;

use std::ffi::OsStr;
use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::output::OutputState;
use duded::ports::{Answered, Ask};
use duded::predicate::{PredicateOutcome, PredicateRunner};
use duded::settings::{RunOptions, Seams};
use duded::startup::Witness;
use duded::{ChildEnds, ChildInput, ChildTurn, ExitCause};
use dudes_host::{ChildExit, ChildStream, ChildStreams, ProtocolWriter, RealHost};
use dudes_proto::rpc::agent::HireParams;
use dudes_proto::{ExitStatus, Record, TurnOutcome};
use dudes_providers::adapter::{
    Adapter, AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
};
use dudes_providers::codex::{CodexAdapter, config::CodexConfig};
use dudes_providers::launch::{EnvVar, LaunchError, LaunchSpec};
use dudes_providers::protocol::ProtocolAction;
use dudes_providers::stream::TurnReader;
use dudes_supervisor::host::{AdoptionKey, Liveness, ProcessHost as _};
use dudes_types::{ModelSelection, Provider, Tier};
use serde_json::{Value, json};

const MARKER: &str = "DUDES_CODEX_PROTOCOL_TEST";
const THREAD: &str = "0190f6d2-6b1a-7c3e-9f00-000000000001";
const TURN: &str = "test-turn";

fn emit(value: &Value) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "{value}").unwrap();
    out.flush().unwrap();
}
fn wait_for(path: &Path) {
    let until = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(Instant::now() < until, "fixture release was not observed");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn protocol_fixture() {
    let Some(marker) = std::env::var_os(MARKER).map(PathBuf::from) else {
        return;
    };
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let input: Value = serde_json::from_str(&line).unwrap();
        let id = input.get("id").cloned().unwrap_or(Value::Null);
        match input["method"].as_str().unwrap() {
            "initialize" if marker.join("stall-init").exists() => loop {
                std::thread::sleep(Duration::from_secs(1));
            },
            "initialize" => emit(
                &json!({"id":id,"result":{"userAgent":"dudes/0.160.0 (fixture)","codexHome":std::env::var("CODEX_HOME").unwrap()}}),
            ),
            "initialized" => {}
            "thread/start" | "thread/resume" => {
                std::fs::write(marker.join("thread.ready"), "").unwrap();
                if marker.join("hold-thread").exists() {
                    wait_for(&marker.join("allow-thread"));
                }
                emit(
                    &json!({"id":id,"result":{"thread":{"id":THREAD,"status":{"type":"idle"}},"cwd":input["params"]["cwd"],"approvalPolicy":"never","sandbox":{"type":"workspaceWrite","writableRoots":[std::env::var("TMPDIR").unwrap(),std::path::PathBuf::from(std::env::var("DUDES_MAILBOX").unwrap()).canonicalize().unwrap().join("outbox")],"networkAccess":false,"excludeTmpdirEnvVar":true,"excludeSlashTmp":true}}}),
                );
            }
            "turn/start" => {
                assert!(
                    input["params"]["input"][0]["text"]
                        .as_str()
                        .unwrap()
                        .starts_with("make the tests pass")
                );
                assert!(
                    !marker.join("task.sent").exists(),
                    "a process received a second task"
                );
                std::fs::write(
                    marker.join("task.sent"),
                    input["params"]["input"][0]["text"].as_str().unwrap(),
                )
                .unwrap();
                emit(&json!({"id":id,"result":{"turn":{"id":TURN,"status":"inProgress"}}}));
                emit(
                    &json!({"method":"item/started","params":{"threadId":THREAD,"turnId":TURN,"item":{"type":"agentMessage","id":"reply"}}}),
                );
                emit(
                    &json!({"method":"item/agentMessage/delta","params":{"threadId":THREAD,"turnId":TURN,"itemId":"reply","delta":"hel"}}),
                );
                wait_for(&marker.join("finish"));
                emit(
                    &json!({"method":"item/agentMessage/delta","params":{"threadId":THREAD,"turnId":TURN,"itemId":"reply","delta":"lo"}}),
                );
                emit(
                    &json!({"method":"item/completed","params":{"threadId":THREAD,"turnId":TURN,"item":{"type":"agentMessage","id":"reply","text":"hello"}}}),
                );
                emit(
                    &json!({"method":"thread/tokenUsage/updated","params":{"threadId":THREAD,"turnId":TURN,"tokenUsage":{"total":{"inputTokens":10,"cachedInputTokens":2,"cacheWriteInputTokens":1,"outputTokens":3}}}}),
                );
                emit(
                    &json!({"method":"turn/completed","params":{"threadId":THREAD,"turn":{"id":TURN,"status":"completed"}}}),
                );
            }
            other => panic!("unsupported fixture request {other}"),
        }
    }
    if marker.join("ignore-eof").exists() {
        std::fs::write(marker.join("eof.seen"), "").unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    std::process::exit(0);
}

struct FixtureAdapter {
    adapter: CodexAdapter,
    program: PathBuf,
}
impl Adapter for FixtureAdapter {
    fn provider(&self) -> Provider {
        Provider::Codex
    }
    fn program(&self) -> &OsStr {
        self.program.as_os_str()
    }
    fn tier(&self) -> Tier {
        self.adapter.tier()
    }
    fn isolation(&self) -> Isolation {
        self.adapter.isolation()
    }
    fn session_acquisition(&self) -> SessionAcquisition {
        self.adapter.session_acquisition()
    }
    fn model_selection(&self) -> ModelSelection {
        self.adapter.model_selection()
    }
    fn managed(
        &self,
        ctx: &AgentCtx<'_>,
        task: &str,
        session: &TurnSession,
        selection: &ModelSelection,
    ) -> Result<LaunchSpec, LaunchError> {
        let mut spec = self.adapter.managed(ctx, task, session, selection)?;
        spec.argv = vec![
            self.program.as_os_str().to_owned(),
            "--exact".into(),
            "protocol_fixture".into(),
            "--nocapture".into(),
            "--quiet".into(),
            "--".into(),
        ];
        Ok(spec)
    }
    fn turn_reader(&self) -> Box<dyn TurnReader> {
        self.adapter.turn_reader()
    }
    fn turn_reader_for(&self, spec: &LaunchSpec) -> Box<dyn TurnReader> {
        self.adapter.turn_reader_for(spec)
    }
    fn managed_install(&self, ctx: &AgentCtx<'_>, selection: &ModelSelection) -> ManagedInstall {
        self.adapter.managed_install(ctx, selection)
    }
    fn usage_source(&self) -> UsageSource {
        self.adapter.usage_source()
    }
    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        self.adapter.sandbox_flag()
    }
    fn billing_env_vars(&self) -> &'static [&'static str] {
        &[]
    }
}

struct HostStreams(Arc<ChildEnds>);
impl ChildStreams for HostStreams {
    fn watch(&self, key: Option<&AdoptionKey>) -> Box<dyn ChildStream> {
        Box::new(HostTurn(self.0.watch(key)))
    }
    fn watch_protocol(
        &self,
        key: &AdoptionKey,
        input: ProtocolWriter,
    ) -> Option<Box<dyn ChildStream>> {
        Some(Box::new(HostTurn(
            self.0.watch_protocol(key, Box::new(Input(input))),
        )))
    }
    fn protocol_failed(&self, key: &AdoptionKey) {
        self.0.protocol_failed(key);
    }
}
#[derive(Debug)]
struct Input(ProtocolWriter);
impl ChildInput for Input {
    fn send(&self, action: ProtocolAction) -> std::io::Result<()> {
        self.0.send(action)
    }
}
struct HostTurn(ChildTurn);
impl ChildStream for HostTurn {
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
struct Runner;
impl PredicateRunner for Runner {
    fn run(&self, spec: &LaunchSpec, _timeout: Duration) -> std::io::Result<PredicateOutcome> {
        let output = if spec
            .argv
            .first()
            .is_some_and(|program| program == "/usr/bin/osascript")
        {
            "{\"synchronized\":true,\"config\":false,\"requirements\":false}"
        } else if spec.argv.last().is_some_and(|arg| arg == "--version") {
            "codex-cli 0.160.0"
        } else {
            ""
        };
        Ok(PredicateOutcome::Finished {
            exit: Some(0),
            output_tail: output.into(),
            timed_out: false,
        })
    }
    fn sweep(&self) {}
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    Stream,
    ChangedConfig,
    InitTimeout,
    CloseTimeout,
}

#[expect(
    clippy::too_many_lines,
    reason = "the test follows one native process from durable issue through incremental output and confirmed cleanup"
)]
fn run(case: Case) {
    let reject_before_task = case == Case::ChangedConfig;
    let fixture = common::Fixture::new();
    let workspace = tempfile::tempdir().unwrap();
    let marker = tempfile::tempdir().unwrap();
    match case {
        Case::ChangedConfig => std::fs::write(marker.path().join("hold-thread"), "").unwrap(),
        Case::InitTimeout => std::fs::write(marker.path().join("stall-init"), "").unwrap(),
        Case::CloseTimeout => std::fs::write(marker.path().join("ignore-eof"), "").unwrap(),
        Case::Stream => {}
    }
    let config = CodexConfig {
        streaming: true,
        command: std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    };
    let adapter = FixtureAdapter {
        adapter: CodexAdapter::new(config.clone())
            .with_managed_home(fixture.state.path().join("providers/codex")),
        program: config.command.clone().into(),
    };
    let settings = duded::settings::Settings {
        daemon_env: vec![EnvVar::new(MARKER, marker.path().as_os_str())],
        ..fixture.settings()
    };
    let children = ChildEnds::new();
    let output = children.output();
    let seams = Seams {
        adapters: [Box::new(adapter) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children: Arc::clone(&children),
        ..common::seams(
            &settings,
            RealHost::watching(Arc::new(HostStreams(children))),
            Arc::new(Runner),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        )
    };
    let options = RunOptions::default().with_providers(duded::config::ProviderTables {
        codex: Some(config),
        ..Default::default()
    });
    let mut running = common::start(duded::boot_with_options(&settings, seams, options).unwrap());
    let agent = match running
        .handle
        .ask(Ask::Hire(Box::new(HireParams {
            provider: Provider::Codex,
            ..common::hire_params(workspace.path())
        })))
        .unwrap()
    {
        Answered::Hired(agent) => agent.agent,
        other => panic!("unexpected {other:?}"),
    };
    let started = Instant::now();
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    if reject_before_task {
        wait_for(&marker.path().join("thread.ready"));
        std::fs::write(
            fixture.state.path().join("providers/codex/config.toml"),
            "unread canary",
        )
        .unwrap();
        std::fs::write(marker.path().join("allow-thread"), "").unwrap();
    } else if case != Case::InitTimeout {
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            if let OutputState::Ready {
                chunks,
                ended: false,
                ..
            } = output.snapshot(agent).state
                && chunks
                    .iter()
                    .map(|chunk| chunk.text.as_str())
                    .collect::<String>()
                    == "hel"
            {
                break;
            }
            assert!(
                Instant::now() < until,
                "live fragment was not visible before the terminal event; records={:?}",
                fixture
                    .records()
                    .iter()
                    .map(Record::kind)
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !fixture
                .records()
                .iter()
                .any(|record| matches!(record, Record::TurnEnded { .. }))
        );
        std::fs::write(marker.path().join("finish"), "").unwrap();
    }
    let until = Instant::now() + Duration::from_secs(40);
    let outcome = loop {
        if let Some(outcome) = fixture.records().iter().find_map(|record| match record {
            Record::TurnEnded {
                outcome: Some(outcome @ (TurnOutcome::Completed | TurnOutcome::Failed { .. })),
                ..
            } => Some(outcome.clone()),
            _ => None,
        }) {
            break outcome;
        }
        assert!(
            Instant::now() < until,
            "native protocol did not reach an observed end"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    if case == Case::Stream {
        assert_eq!(outcome, TurnOutcome::Completed);
        if let OutputState::Ready { chunks, .. } = output.snapshot(agent).state {
            assert_eq!(
                chunks
                    .iter()
                    .map(|chunk| chunk.text.as_str())
                    .collect::<String>(),
                "hello"
            );
        } else {
            panic!("no completed output");
        }
    } else {
        assert!(matches!(outcome, TurnOutcome::Failed { .. }));
        if case != Case::CloseTimeout {
            assert!(!marker.path().join("task.sent").exists());
        }
        if case == Case::InitTimeout {
            assert!(started.elapsed() >= Duration::from_secs(30));
        }
        if case == Case::CloseTimeout {
            assert!(marker.path().join("eof.seen").exists());
            assert!(started.elapsed() >= Duration::from_secs(5));
        }
    }
    let records = fixture.records();
    if marker.path().join("task.sent").exists() {
        let task = records
            .iter()
            .find_map(|record| match record {
                Record::TurnIssued { task, .. } => Some(task),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(marker.path().join("task.sent")).unwrap(),
            *task
        );
    }
    let key = records
        .iter()
        .find_map(|record| match record {
            Record::TurnSpawned {
                pid,
                start_time,
                boot,
                ..
            } => Some(AdoptionKey {
                pid: *pid,
                start_time: *start_time,
                boot: boot.clone(),
            }),
            _ => None,
        })
        .unwrap();
    assert_eq!(RealHost::new().alive(&key), Liveness::Gone);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn daemon_exposes_incremental_text_before_provider_completion() {
    run(Case::Stream);
}
#[test]
fn configuration_added_during_startup_prevents_task_delivery() {
    run(Case::ChangedConfig);
}

#[test]
fn stalled_initialization_reaches_the_real_startup_deadline_and_reaps_the_child() {
    run(Case::InitTimeout);
}
#[test]
fn ignored_eof_reaches_the_real_close_deadline_and_reaps_the_child() {
    run(Case::CloseTimeout);
}
