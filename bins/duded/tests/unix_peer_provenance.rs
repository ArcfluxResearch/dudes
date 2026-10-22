//! Production resolver, managed-child, attached-child and refusal controls for Unix peer
//! provenance.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::ffi::OsString;
#[cfg(target_os = "macos")]
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use duded::classify::{ExternalChild, PeerBarrier, ProcessRelation, Relation, Resolver};
use duded::ports::{
    Answered, Ask, OpenedPty, PtyChild, PtyCloseOutcome, PtyHostPort, PtyOpenFailure,
};
use duded::pty_output::OutputSubscription;
use duded::settings::Seams;
use duded::startup::Witness;
use duded::{ChildEnds, ChildTurn, ExitCause, boot};
use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_journal::{Fsync, JournalOptions};
use dudes_proto::providers::claude::Settings;
use dudes_proto::rpc::agent::{AskParams, AttachParams};
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::hook::HookToken;
use dudes_proto::rpc::pty::PtyId;
use dudes_proto::rpc::{Empty, ErrorCode, Method, Response};
use dudes_proto::{Actor, AgentBudget, Entry, ExitStatus, Record};
use dudes_providers::adapter::{
    Adapter, AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
};
use dudes_providers::claude::SESSION_ACQUISITION;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::claude::stream::TurnStream;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_providers::registry::{Adapters, ProvidersConfig};
use dudes_providers::stream::TurnReader;
use dudes_supervisor::Input;
use dudes_supervisor::blake3_hex;
use dudes_supervisor::host::AdoptionKey;
use dudes_supervisor::testing::FakeRng;
use dudes_types::{AgentId, ModelSelection, Provider, Tier, Timestamp, Usd};
use serde_json::{Value, json};

mod common;

use common::hook_shim::{handler, handlers_for, hook_binary, payload, spawn_handler};
use common::{Fixture, ScriptedRunner, SharedHost, TestClock};

const ROLE: &str = "DUDES_UNIX_PROVENANCE_TEST_ROLE";
const RELEASE: &str = "DUDES_UNIX_PROVENANCE_TEST_RELEASE";
const ASK: &str = "ask";
const PARK: &str = "park";
const DEAD_PEER: &str = "dead-peer";
const ATTACHED_HOOK: &str = "attached-hook";
const ATTACHED_OPERATOR: &str = "attached-operator";
const ATTACHED_SETTINGS: &str = "DUDES_UNIX_PROVENANCE_TEST_SETTINGS";
const ATTACHED_HOOK_COMMAND: &str = "DUDES_UNIX_PROVENANCE_TEST_HOOK_COMMAND";
const ATTACHED_OBSERVED: &str = "DUDES_UNIX_PROVENANCE_TEST_OBSERVED";
const ATTACHED_CREDENTIAL: &str = "DUDES_UNIX_PROVENANCE_TEST_CREDENTIAL";
const SESSION_INIT: &str = r#"{"type":"system","subtype":"init","session_id":"3f9a1c2e-0000-4000-8000-00000000463a","tools":[]}"#;
const COMPLETED_RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"session_id":"3f9a1c2e-0000-4000-8000-00000000463a","result":"provenance helper completed","total_cost_usd":0.0,"modelUsage":{}}"#;
#[cfg(target_os = "macos")]
const PEER_SOCKET: &str = "DUDES_UNIX_PROVENANCE_TEST_PEER_SOCKET";
const QUESTION: &str = "which verified step should run next?";
const MATRIX_TOKEN: &str = "mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm";

fn matrix_agent() -> AgentId {
    AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("matrix agent")
}

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_789_249_200_000),
        record,
    }
}

fn seed_matrix_agent(fixture: &Fixture) {
    let options = JournalOptions {
        fsync: Fsync::None,
        checkpoint_every: 0,
        ..JournalOptions::default()
    };
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), options).expect("open matrix journal");
    writer
        .append(&entry(Record::AgentHired {
            character: None,
            accent: None,
            id: matrix_agent(),
            name: "Matrix".to_owned(),
            role: "backend engineer".to_owned(),
            department: "engineering".to_owned(),
            provider: Provider::Claude,
            cwd: fixture.state.path().to_path_buf(),
            budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            trusted: false,
            workspace: None,
            worktree: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        }))
        .expect("seed matrix agent");
    writer
        .append(&entry(Record::HookTokenMinted {
            agent: matrix_agent(),
            token_hash: blake3_hex(MATRIX_TOKEN.as_bytes()),
        }))
        .expect("seed matrix token");
}

/// Test-local copy of `main.rs`'s concrete host adapter.
#[derive(Debug)]
struct HostPeerProcesses(dudes_host::PeerProcesses);

impl ProcessRelation for HostPeerProcesses {
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
        let relation = match pidversion {
            Some(pidversion) => self.0.resolve_execution(peer, pidversion, children),
            None => dudes_host::PeerRelation::Unresolvable,
        };
        #[cfg(target_os = "linux")]
        let relation = {
            let _ = pidversion;
            self.0.resolve(peer, children)
        };
        match relation {
            dudes_host::PeerRelation::Matched(index) => Relation::Matched(index),
            dudes_host::PeerRelation::Pending(slot) => Relation::Pending(slot),
            dudes_host::PeerRelation::NoMatch => Relation::NoMatch,
            dudes_host::PeerRelation::Unresolvable => Relation::Unresolvable,
            dudes_host::PeerRelation::Abandoned => Relation::Abandoned,
        }
    }

    fn track_external(&self, slot: u64) -> Box<dyn ExternalChild> {
        Box::new(HostExternalChild(self.0.track_external(slot)))
    }
}

/// Test-local copy of `main.rs`'s relation hold on an attached child.
#[derive(Debug)]
struct HostExternalChild(dudes_host::ExternalChild);

impl ExternalChild for HostExternalChild {
    fn register(&mut self, child: PtyChild) -> std::io::Result<AdoptionKey> {
        self.0.register(child.pid)
    }
}

#[test]
fn the_helper_entry_point() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    match role.as_str() {
        ASK => ask_as_managed_child(),
        PARK => park_until_released(),
        DEAD_PEER => wait_to_become_a_dead_peer(),
        ATTACHED_HOOK => run_pre_tool_hooks_as_the_attached_cli(),
        ATTACHED_OPERATOR => present_the_operator_credential_from_the_attached_cli(),
        other => panic!("unknown helper role {other}"),
    }
    write_completed_result();
    std::process::exit(0);
}

#[cfg(target_os = "macos")]
fn wait_to_become_a_dead_peer() {
    use std::io::Read as _;

    let socket = PathBuf::from(std::env::var_os(PEER_SOCKET).expect("peer socket"));
    let mut stream = UnixStream::connect(socket).expect("connect dead-peer helper");
    let mut release = [0];
    stream
        .read_exact(&mut release)
        .expect("wait for identity capture");
    std::process::exit(0);
}

#[cfg(target_os = "linux")]
fn wait_to_become_a_dead_peer() {
    unreachable!("the native dead-peer control is macOS-only")
}

fn agent_env() -> (PathBuf, HookToken) {
    let socket = PathBuf::from(std::env::var_os("DUDES_SOCK").expect("agent socket"));
    let token = HookToken::new(
        std::env::var("DUDES_TOKEN").expect("agent token is present but never printed"),
    )
    .expect("agent token shape");
    (socket, token)
}

fn ask_as_managed_child() {
    let (socket, token) = agent_env();
    let params = serde_json::to_value(AskParams {
        token,
        text: QUESTION.to_owned(),
    })
    .expect("typed AgentAsk params");
    let mut client = dudes_rpc::Client::connect(socket).expect("managed child connects");
    let response = client
        .call(Method::AgentAsk, Some(params))
        .expect("managed AgentAsk is answered");
    assert!(
        response.error.is_none(),
        "managed AgentAsk was refused: {:?}",
        response.error
    );
    let result: Empty = serde_json::from_value(response.result.expect("AgentAsk result"))
        .expect("AgentAsk returns Empty");
    assert_eq!(result, Empty {});
}

fn park_until_released() {
    let release = PathBuf::from(std::env::var_os(RELEASE).expect("release path"));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !release.exists() {
        assert!(Instant::now() < deadline, "park helper was never released");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The attached CLI's agent and the `--settings` file it was launched with.
fn attached_cli() -> (AgentId, PathBuf) {
    let settings = PathBuf::from(std::env::var_os(ATTACHED_SETTINGS).expect("--settings path"));
    let agent = settings
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| AgentId::parse(name.to_str()?).ok())
        .expect("the settings file is agents/<id>/hooks/settings.json");
    (agent, settings)
}

fn observed_dir() -> PathBuf {
    PathBuf::from(std::env::var_os(ATTACHED_OBSERVED).expect("observation directory"))
}

fn observation_path(dir: &Path, agent: AgentId, what: &str) -> PathBuf {
    dir.join(format!("{agent}-{what}.json"))
}

fn release_path(dir: &Path, agent: AgentId, hook: u32) -> PathBuf {
    dir.join(format!("{agent}-{hook}.go"))
}

fn publish_observation(path: &Path, observed: &Value) {
    let partial = path.with_extension("partial");
    std::fs::write(&partial, observed.to_string()).expect("write the observation");
    std::fs::rename(&partial, path).expect("publish the observation");
}

/// Claude Code runs an installed hook handler as its own child, from the `--settings` file it
/// was launched with. The first hook runs at once, and each later one when the test releases it.
fn run_pre_tool_hooks_as_the_attached_cli() {
    let (agent, settings) = attached_cli();
    let dir = observed_dir();
    for hook in 0.. {
        if hook > 0 && !released(&release_path(&dir, agent, hook)) {
            return;
        }
        let installed: Option<Settings> = std::fs::read_to_string(&settings)
            .ok()
            .map(|text| serde_json::from_str(&text).expect("installed hook settings"));
        let agent_dir = settings
            .parent()
            .and_then(Path::parent)
            .expect("the agent directory");
        let hook_command = std::env::var_os(ATTACHED_HOOK_COMMAND).expect("hook command");
        let would_install = handlers_for(agent, agent_dir, Path::new(&hook_command));
        let pre_tool = handler(installed.as_ref().unwrap_or(&would_install), "PreToolUse");
        let (socket, token) = agent_env();
        let (pid, output) = spawn_handler(pre_tool, &socket, &token, &payload("PreToolUse"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        publish_observation(
            &observation_path(&dir, agent, &hook.to_string()),
            &json!({
                "pid": pid,
                "result": {
                    "settings_installed": installed.is_some(),
                    "exit": output.status.code(),
                    "stdout": serde_json::from_str::<Value>(&stdout).unwrap_or_else(|_| json!(stdout)),
                    "stderr": String::from_utf8_lossy(&output.stderr),
                },
            }),
        );
    }
}

fn released(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// A `dudes` operator command run inside the attached CLI presents the operator credential.
fn present_the_operator_credential_from_the_attached_cli() {
    let (agent, _) = attached_cli();
    let (socket, _) = agent_env();
    let credential = std::fs::read_to_string(
        std::env::var_os(ATTACHED_CREDENTIAL).expect("operator credential path"),
    )
    .expect("operator credential");
    let with_credential = dudes_rpc::Client::connect(&socket)
        .expect("the attached CLI connects")
        .call_as_operator(
            Method::SystemPing,
            None,
            OperatorCredential::new(credential).expect("credential shape"),
        )
        .expect("answered");
    let without = dudes_rpc::Client::connect(&socket)
        .expect("the attached CLI connects")
        .call(Method::SystemPing, None)
        .expect("answered");
    publish_observation(
        &observation_path(&observed_dir(), agent, "operator"),
        &json!({
            "with_credential": with_credential.error.map(|error| error.code),
            "without": without.error.map(|error| error.code),
        }),
    );
}

fn write_completed_result() {
    use std::io::Write as _;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "{COMPLETED_RESULT}").expect("write managed result");
    out.flush().expect("flush managed result");
}

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

    fn billing_env_vars(&self) -> &'static [&'static str] {
        &[]
    }
}

fn helper_argv() -> Vec<OsString> {
    let mut argv = vec![OsString::from(
        std::env::current_exe().expect("current test executable"),
    )];
    argv.extend(["--exact", "the_helper_entry_point", "--quiet"].map(OsString::from));
    argv
}

fn daemon_env(role: &str, release: &Path) -> Vec<EnvVar> {
    let mut env: Vec<_> = ["PATH", "HOME", "TMPDIR"]
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| EnvVar::new(*name, value)))
        .collect();
    env.push(EnvVar::new(ROLE, role));
    env.push(EnvVar::new(RELEASE, release.as_os_str()));
    env
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

#[derive(Debug)]
struct HostAnswer {
    peer: u32,
    recorded_children: usize,
    relation: Relation,
}

#[derive(Debug)]
struct RecordedHostPeerProcesses {
    host: HostPeerProcesses,
    answers: Mutex<Vec<HostAnswer>>,
}

impl RecordedHostPeerProcesses {
    fn answers(&self) -> Vec<String> {
        self.answers
            .lock()
            .expect("recorded answers")
            .iter()
            .map(|answer| {
                format!(
                    "pid {} against {} recorded children: {:?}",
                    answer.peer, answer.recorded_children, answer.relation
                )
            })
            .collect()
    }
}

impl ProcessRelation for RecordedHostPeerProcesses {
    fn begin_spawn(&self) -> u64 {
        self.host.begin_spawn()
    }

    fn finish_spawn(&self, slot: u64, key: Option<&AdoptionKey>) -> bool {
        self.host.finish_spawn(slot, key)
    }

    fn published(&self, children: &[AdoptionKey]) -> Vec<(u64, usize)> {
        self.host.published(children)
    }

    fn track_external(&self, slot: u64) -> Box<dyn ExternalChild> {
        self.host.track_external(slot)
    }

    fn resolve(&self, peer: u32, pidversion: Option<u32>, children: &[AdoptionKey]) -> Relation {
        let relation = self.host.resolve(peer, pidversion, children);
        self.answers
            .lock()
            .expect("recorded answers")
            .push(HostAnswer {
                peer,
                recorded_children: children.len(),
                relation,
            });
        relation
    }
}

/// A native PTY host the test shares with the daemon. While `refuse_close` is set, the daemon's
/// closes fail and leave the child running, as a close the platform could not confirm does.
#[derive(Clone, Default)]
struct NativePtys {
    host: Arc<Mutex<dudes_pty::PtyHost>>,
    readers: Arc<Mutex<Vec<dudes_pty::PtyReader>>>,
    opened: Arc<Mutex<Vec<PtyId>>>,
    refuse_close: Arc<AtomicBool>,
}

impl NativePtys {
    fn close_for_real(&self, pty: PtyId) {
        let closed = self.host.lock().expect("PTY host").close(pty);
        assert!(closed.is_ok(), "the test's own close: {closed:?}");
    }

    fn live(&self, pty: PtyId) -> bool {
        self.host.lock().expect("PTY host").pid(pty).is_some()
    }
}

impl PtyHostPort for NativePtys {
    fn open(
        &mut self,
        spec: LaunchSpec,
        cols: u16,
        rows: u16,
    ) -> Result<OpenedPty, PtyOpenFailure> {
        let mut host = self.host.lock().expect("PTY host");
        match host.open(spec, dudes_pty::PtySize { cols, rows }) {
            Ok(opened) => {
                self.readers.lock().expect("readers").push(opened.reader);
                self.opened.lock().expect("opened").push(opened.id);
                let pid = host.pid(opened.id).expect("an opened PTY runs a child");
                Ok(OpenedPty {
                    pty: opened.id,
                    child: PtyChild { pid },
                })
            }
            Err(dudes_pty::OpenError::Failed(error)) => Err(PtyOpenFailure::Failed(error)),
            Err(dudes_pty::OpenError::CleanupUnconfirmed {
                pty,
                cause,
                cleanup,
            }) => Err(PtyOpenFailure::CleanupUnconfirmed {
                pty,
                cause,
                cleanup,
            }),
        }
    }

    fn write(&mut self, pty: PtyId, bytes: &[u8]) -> std::io::Result<usize> {
        self.host.lock().expect("PTY host").write(pty, bytes)
    }

    fn resize(&mut self, pty: PtyId, cols: u16, rows: u16) -> std::io::Result<()> {
        self.host
            .lock()
            .expect("PTY host")
            .resize(pty, dudes_pty::PtySize { cols, rows })
    }

    fn subscribe(&mut self, _pty: PtyId) -> Option<OutputSubscription> {
        None
    }

    fn close(&mut self, pty: PtyId) -> PtyCloseOutcome {
        if self.refuse_close.load(Ordering::SeqCst) {
            return PtyCloseOutcome::Failed(std::io::Error::other("injected close refusal"));
        }
        match self.host.lock().expect("PTY host").close(pty) {
            Ok(dudes_pty::CloseOutcome::Closed) => PtyCloseOutcome::Closed,
            Ok(dudes_pty::CloseOutcome::AlreadyClosed) => PtyCloseOutcome::AlreadyClosed,
            Err(error) => PtyCloseOutcome::Failed(error),
        }
    }

    fn shutdown(&mut self) -> std::io::Result<()> {
        if self.refuse_close.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("injected shutdown refusal"));
        }
        self.host.lock().expect("PTY host").shutdown()?;
        self.readers.lock().expect("readers").clear();
        Ok(())
    }
}

fn start_real_with(
    settings: &duded::settings::Settings,
    pty_host: Box<dyn PtyHostPort>,
    adapters: Adapters,
) -> (common::Running, Arc<RecordedHostPeerProcesses>) {
    let children = ChildEnds::new();
    let host = RealHost::watching(Arc::new(HostStreams(Arc::clone(&children))));
    let processes = Arc::new(RecordedHostPeerProcesses {
        host: HostPeerProcesses(host.peer_processes()),
        answers: Mutex::new(Vec::new()),
    });
    let resolver = Resolver::processes(Arc::clone(&processes) as Arc<dyn ProcessRelation>);
    let host = PeerBarrier::new(host, resolver.clone());
    let seams = Seams {
        host,
        pty_host,
        predicates: Arc::new(ScriptedRunner::scripted([common::passes()])),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(settings, FakeRng::new()),
        resolver,
        adapters,
        children,
        witness: Witness::default(),
    };
    let running = common::start(boot(settings, seams).expect("boot with production resolver"));
    (running, processes)
}

fn start_real(fixture: &Fixture, role: &str, release: &Path) -> common::Running {
    let settings = duded::settings::Settings {
        daemon_env: daemon_env(role, release),
        ..fixture.settings()
    };
    let adapters = [Box::new(HelperAdapter) as Box<dyn Adapter>]
        .into_iter()
        .collect();
    start_real_with(&settings, Box::new(duded::ports::ClosedPtyHost), adapters).0
}

fn questions(fixture: &Fixture) -> Vec<(AgentId, String)> {
    fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::HumanQuestion { agent, text } => Some((agent, text)),
            _ => None,
        })
        .collect()
}

fn mint_for(
    fixture: &Fixture,
    tokens: &dudes_supervisor::testing::FakeTokens,
    agent: AgentId,
) -> HookToken {
    let hash = fixture.await_record("HookTokenMinted", |record| match record {
        Record::HookTokenMinted {
            agent: who,
            token_hash,
        } if *who == agent => Some(token_hash.clone()),
        _ => None,
    });
    tokens.token_of(&hash)
}

#[test]
fn managed_child_uses_shipped_resolver_for_agent_ask_rpc() {
    let fixture = Fixture::new();
    let release = fixture.state.path().join("unused-release");
    let mut running = start_real(&fixture, ASK, &release);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );

    let observed =
        fixture.await_record(
            "managed HumanQuestion or early turn end",
            |record| match record {
                Record::HumanQuestion { agent: who, text } if *who == agent && text == QUESTION => {
                    Some(Ok(()))
                }
                Record::TurnEnded { agent: who, .. } if *who == agent => Some(Err(record.clone())),
                _ => None,
            },
        );
    if let Err(ended) = observed {
        panic!("managed child ended before AgentAsk reached the journal: {ended:?}");
    }
    assert_eq!(
        questions(&fixture),
        vec![(agent, QUESTION.to_owned())],
        "the managed child produces exactly one durably attributed question"
    );
    assert_eq!(running.stop(ExitCause::Ended), ExitCause::Ended);
}

/// The host reports a child's exit only after it has released the child's peer entry, so a
/// resolve after the report sees that release.
#[derive(Clone)]
struct Exits(mpsc::Sender<ChildExit>);

impl ChildStreams for Exits {
    fn watch(&self, _key: Option<&AdoptionKey>) -> Box<dyn ChildStream> {
        Box::new(self.clone())
    }
}

impl ChildStream for Exits {
    fn line(&mut self, _text: &str) {}

    fn ended(self: Box<Self>, exit: ChildExit) {
        let _ = self.0.send(exit);
    }
}

#[test]
fn an_unrelated_peer_keeps_its_column_after_a_managed_child_exits_on_its_own() {
    use dudes_rpc::auth::PeerResolution;
    use dudes_rpc::{PeerProvenance, PeerResolver as _};
    use dudes_supervisor::host::{ProcessHost as _, SpawnResult};

    let dir = tempfile::tempdir().expect("managed child directory");
    let release = dir.path().join("release-park");
    std::fs::write(&release, b"release").expect("let the managed child exit at once");
    let (exits, exited) = mpsc::channel();
    let host = RealHost::watching(Arc::new(Exits(exits)));
    let resolver = Resolver::processes(Arc::new(HostPeerProcesses(host.peer_processes())));
    let mut host = PeerBarrier::new(host, resolver.clone());
    let (this_process, _other_end) = UnixStream::pair().expect("identity pair");
    let unrelated = dudes_rpc::peer::identity(&this_process);
    let no_recorded_agent = PeerResolution::Resolved(PeerProvenance::NoRecordedAgent);

    assert_eq!(
        resolver.resolve(&unrelated),
        no_recorded_agent,
        "before any managed child"
    );

    let spec = LaunchSpec {
        argv: helper_argv(),
        env: daemon_env(PARK, &release),
        cwd: dir.path().to_path_buf(),
        stdin: dudes_providers::launch::LaunchInput::Null,
    };
    let SpawnResult::Spawned(_) = host.spawn(&spec, &ManagedInstall::default()) else {
        panic!("the managed child did not spawn");
    };
    let exit = exited
        .recv_timeout(Duration::from_secs(20))
        .expect("the managed child exits on its own");
    assert_eq!(exit.code, Some(0), "the managed child finished cleanly");

    assert_eq!(
        resolver.resolve(&unrelated),
        no_recorded_agent,
        "after the managed child exited on its own"
    );
}

fn claude_stand_in(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let path = dir.join("claude-stand-in");
    let turns = dir.join("stand-in-turns");
    let helper = std::env::current_exe().expect("current test executable");
    // Each managed turn names a session of its own: a vendor session bound to one agent is never
    // learned for another.
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = --resume ] && [ "$3" = --settings ]; then
  export {ATTACHED_SETTINGS}="$4"
  exec '{}' --exact the_helper_entry_point --quiet
fi
n=$(( $(cat '{}' 2>/dev/null || echo 0) + 1 ))
echo "$n" > '{}'
printf '%s\n' '{SESSION_INIT}' '{COMPLETED_RESULT}' | sed "s/00000000463a/$(printf '%012d' "$n")/"
"#,
        helper.display(),
        turns.display(),
        turns.display(),
    );
    std::fs::write(&path, script).expect("write the stand-in");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .expect("the stand-in is executable");
    path
}

fn await_observation(fixture: &Fixture, agent: AgentId, what: &str) -> Value {
    let path = observation_path(&fixture.state.path().join("observed"), agent, what);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(text) = std::fs::read_to_string(&path) {
            return serde_json::from_str(&text).expect("observation JSON");
        }
        assert!(
            Instant::now() < deadline,
            "the attached CLI never reported {what}; records: {:?}",
            fixture
                .records()
                .iter()
                .map(Record::kind)
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn release_hook(fixture: &Fixture, agent: AgentId, hook: u32) {
    std::fs::write(
        release_path(&fixture.state.path().join("observed"), agent, hook),
        b"",
    )
    .expect("release the next hook");
}

/// What a paused agent's `PreToolUse` hook answers once its peer resolves to that agent.
fn denied_while_paused() -> Value {
    json!({
        "settings_installed": true,
        "exit": 2,
        "stdout": {"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": "the loop is paused"
        }},
        "stderr": "",
    })
}

fn attached_settings(fixture: &Fixture, role: &str) -> duded::settings::Settings {
    let observed = fixture.state.path().join("observed");
    std::fs::create_dir_all(&observed).expect("observation directory");
    let settings = fixture.settings();
    let mut daemon_env = daemon_env(role, &fixture.state.path().join("unused-release"));
    daemon_env.push(EnvVar::new(
        ATTACHED_HOOK_COMMAND,
        hook_binary().as_os_str(),
    ));
    daemon_env.push(EnvVar::new(ATTACHED_OBSERVED, observed.as_os_str()));
    daemon_env.push(EnvVar::new(
        ATTACHED_CREDENTIAL,
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME)
            .as_os_str(),
    ));
    duded::settings::Settings {
        daemon_env,
        hook_command: Some(hook_binary().to_path_buf()),
        ..settings
    }
}

/// A daemon on the shipped resolver whose Claude adapter launches the stand-in, and two operator
/// connections that hold attach authority because they were accepted before any hire. A
/// connection owns at most one attachment, so each attach takes the next one.
struct AttachedDaemon {
    settings: duded::settings::Settings,
    running: common::Running,
    processes: Arc<RecordedHostPeerProcesses>,
    operators: Vec<dudes_rpc::LocalClient>,
    attached: Vec<AgentId>,
}

impl AttachedDaemon {
    fn start(
        fixture: &Fixture,
        settings: &duded::settings::Settings,
        ptys: Box<dyn PtyHostPort>,
    ) -> Self {
        let stand_in = ClaudeConfig {
            command: claude_stand_in(fixture.state.path()).into_os_string(),
            ..ClaudeConfig::default()
        };
        let adapters = Adapters::from_config(
            &ProvidersConfig {
                claude: Some(stand_in),
            },
            &settings.daemon_env,
        )
        .expect("stand-in Claude adapter");
        let (running, processes) = start_real_with(settings, ptys, adapters);
        let operators = (0..2)
            .map(|_| {
                let mut operator =
                    dudes_rpc::Client::connect(settings.socket()).expect("operator connects");
                let ping = operator
                    .call(Method::SystemPing, None)
                    .expect("attach authority is leased only to a connection accepted before hire");
                assert!(ping.error.is_none(), "ping: {ping:?}");
                operator
            })
            .collect();
        Self {
            settings: settings.clone(),
            running,
            processes,
            operators,
            attached: Vec::new(),
        }
    }

    /// Close the operator connections, then stop the daemon and wait for its exit sequence.
    fn stop(&mut self) -> ExitCause {
        self.operators.clear();
        self.running.stop(ExitCause::Ended)
    }

    /// Hire an agent, let its managed turn confirm a session its attach can resume, and pause it.
    fn paused_agent(&self, fixture: &Fixture) -> AgentId {
        let cwd = tempfile::tempdir_in(fixture.state.path())
            .expect("agent working directory")
            .keep();
        let agent = common::hire(&self.running.handle, &cwd);
        let goal = common::set_goal(
            &self.running.handle,
            common::goal_params(agent, &["/usr/bin/true"]),
        );
        fixture.await_record("passing PredicateResult", |record| {
            matches!(record, Record::PredicateResult { goal: judged, pass: true, .. } if *judged == goal)
                .then_some(())
        });
        let paused = self.running.handle.ask(Ask::Pause(agent));
        assert!(matches!(paused, Ok(Answered::Done)), "pause: {paused:?}");
        agent
    }

    fn as_operator(&mut self, connection: usize, method: Method, params: Value) -> Response {
        let credential = std::fs::read_to_string(
            self.settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .expect("operator credential");
        self.operators[connection]
            .call_as_operator(
                method,
                Some(params),
                OperatorCredential::new(credential).expect("credential shape"),
            )
            .expect("answered")
    }

    fn attach(&mut self, agent: AgentId) -> Response {
        let connection = self.attached.len();
        self.attached.push(agent);
        let params = serde_json::to_value(AttachParams {
            agent,
            cols: 80,
            rows: 24,
        })
        .expect("attach params");
        self.as_operator(connection, Method::AgentAttach, params)
    }
}

fn pty_of(fixture: &Fixture, agent: AgentId) -> PtyId {
    fixture.await_record("PtySpawned", |record| match record {
        Record::PtySpawned {
            agent: spawned,
            pty,
            ..
        } if *spawned == agent => Some(*pty),
        _ => None,
    })
}

fn detached_by_system(records: &[Record], agent: AgentId) -> bool {
    records.iter().any(|record| {
        matches!(
            record,
            Record::Detached { agent: detached, by: Actor::System, .. } if *detached == agent
        )
    })
}

/// Across a daemon that died without closing its PTYs: a child that outlived it stays
/// its agent's after the restart, and is detached by the system once a probe reads it gone; one
/// already gone at startup is detached then.
#[test]
fn a_restart_keeps_a_surviving_child_governed_and_detaches_a_gone_one() {
    let fixture = Fixture::new();
    let settings = attached_settings(&fixture, ATTACHED_HOOK);
    let ptys = NativePtys::default();
    let mut first = AttachedDaemon::start(&fixture, &settings, Box::new(ptys.clone()));
    let survivor = first.paused_agent(&fixture);
    let gone = first.paused_agent(&fixture);
    for agent in [survivor, gone] {
        let attach = first.attach(agent);
        assert!(attach.error.is_none(), "attach was refused: {attach:?}");
        assert_eq!(
            await_observation(&fixture, agent, "0")["result"],
            denied_while_paused(),
            "control: governed in the first run"
        );
    }
    let survivor_pty = pty_of(&fixture, survivor);
    let gone_pty = pty_of(&fixture, gone);

    // The first daemon stops without confirming either close, as one killed by `SIGKILL` would.
    ptys.refuse_close.store(true, Ordering::SeqCst);
    let _ = first.stop();
    drop(first);
    let records = fixture.records();
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::Detached { .. })),
        "the first run left both agents attached"
    );
    ptys.close_for_real(gone_pty);
    assert!(ptys.live(survivor_pty), "the survivor outlived its daemon");

    let mut second =
        AttachedDaemon::start(&fixture, &settings, Box::new(duded::ports::ClosedPtyHost));
    let records = fixture.records();
    assert!(
        detached_by_system(&records, gone),
        "startup detaches the agent whose child is gone"
    );
    assert!(
        !detached_by_system(&records, survivor),
        "startup keeps the agent whose child survived"
    );

    release_hook(&fixture, survivor, 1);
    let after_restart = await_observation(&fixture, survivor, "1");
    assert_eq!(
        after_restart["result"],
        denied_while_paused(),
        "hook pid {}; host answers: {:?}",
        after_restart["pid"],
        second.processes.answers()
    );

    ptys.close_for_real(survivor_pty);
    second.running.handle.input(Input::Tick);
    fixture.await_record("the survivor's Detached", |record| {
        matches!(
            record,
            Record::Detached { agent, by: Actor::System, .. } if *agent == survivor
        )
        .then_some(())
    });
    assert_eq!(second.stop(), ExitCause::Ended);
}

/// An operator credential presented from inside an attached tree is
/// the agent's column, so row 1 refuses it; the same peer presenting nothing is still served.
#[test]
fn an_operator_credential_from_inside_an_attached_tree_is_refused() {
    let fixture = Fixture::new();
    let settings = attached_settings(&fixture, ATTACHED_OPERATOR);
    let mut daemon = AttachedDaemon::start(&fixture, &settings, Box::new(NativePtys::default()));
    let agent = daemon.paused_agent(&fixture);
    let attach = daemon.attach(agent);
    assert!(attach.error.is_none(), "attach was refused: {attach:?}");
    let observed = await_observation(&fixture, agent, "operator");
    let control = daemon.operators[1]
        .call(Method::SystemPing, None)
        .expect("answered");
    assert_eq!(daemon.stop(), ExitCause::Ended);

    assert!(
        control.error.is_none(),
        "control: the operator outside the tree is served"
    );
    assert_eq!(
        observed,
        json!({
            "with_credential": ErrorCode::Unauthorized.code(),
            "without": null,
        }),
        "host answers: {:?}",
        daemon.processes.answers()
    );
}

#[derive(Debug)]
struct FixedRelation {
    relation: Relation,
    finish: bool,
}

impl ProcessRelation for FixedRelation {
    fn begin_spawn(&self) -> u64 {
        1
    }

    fn finish_spawn(&self, _slot: u64, _key: Option<&AdoptionKey>) -> bool {
        self.finish
    }

    fn published(&self, _children: &[AdoptionKey]) -> Vec<(u64, usize)> {
        Vec::new()
    }

    fn resolve(&self, _peer: u32, _pidversion: Option<u32>, _children: &[AdoptionKey]) -> Relation {
        self.relation
    }
}

#[derive(Clone, Copy)]
enum NegativeDispatch {
    Completed,
    ConnectionRefused,
}

fn dispatch_matrix_negative(label: &str, relation: Relation, finish: bool) {
    let resolver = Resolver::processes(Arc::new(FixedRelation { relation, finish }));
    let expected = match relation {
        Relation::NoMatch | Relation::Unresolvable => NegativeDispatch::Completed,
        Relation::Pending(_) | Relation::Abandoned => NegativeDispatch::ConnectionRefused,
        Relation::Matched(_) => panic!("{label}: a negative matrix case cannot be Matched"),
    };
    dispatch_negative_with_resolver(label, resolver, expected);
}

fn dispatch_negative_with_resolver(label: &str, resolver: Resolver, expected: NegativeDispatch) {
    let fixture = Fixture::new();
    seed_matrix_agent(&fixture);
    let settings = fixture.settings();
    let seams = Seams {
        host: SharedHost::new(),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(ScriptedRunner::default()),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
        resolver,
        adapters: [Box::new(HelperAdapter) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children: ChildEnds::new(),
        witness: Witness::default(),
    };
    let mut running = common::start(boot(&settings, seams).expect("boot matrix daemon"));
    let params = serde_json::to_value(AskParams {
        token: HookToken::new(MATRIX_TOKEN).expect("matrix token"),
        text: format!("{label} must not ask"),
    })
    .expect("typed negative AgentAsk params");
    let mut client = dudes_rpc::Client::connect(settings.socket()).expect("matrix peer connects");
    let answer = client.call(Method::AgentAsk, Some(params));
    match expected {
        NegativeDispatch::Completed => {
            let response = answer.expect("a completed negative relation is answered");
            assert_eq!(
                response.error.as_ref().map(|error| error.code),
                Some(ErrorCode::Unauthorized.code()),
                "{label}: completed relation reached the wrong dispatch result"
            );
        }
        NegativeDispatch::ConnectionRefused => {
            assert!(
                answer.is_err(),
                "{label}: an incomplete relation read or answered the request"
            );
        }
    }
    assert!(
        questions(&fixture).is_empty(),
        "{label}: the negative relation produced a HumanQuestion"
    );
    assert_eq!(running.stop(ExitCause::Ended), ExitCause::Ended);
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct CapturedHostPeerProcesses {
    host: HostPeerProcesses,
    captured: dudes_rpc::PeerIdentity,
}

#[cfg(target_os = "macos")]
impl ProcessRelation for CapturedHostPeerProcesses {
    fn begin_spawn(&self) -> u64 {
        self.host.begin_spawn()
    }

    fn finish_spawn(&self, slot: u64, key: Option<&AdoptionKey>) -> bool {
        self.host.finish_spawn(slot, key)
    }

    fn published(&self, children: &[AdoptionKey]) -> Vec<(u64, usize)> {
        self.host.published(children)
    }

    fn resolve(&self, _peer: u32, _pidversion: Option<u32>, children: &[AdoptionKey]) -> Relation {
        let dudes_rpc::PeerPid::Reported { pid, pidversion } = self.captured.pid else {
            return Relation::Unresolvable;
        };
        let Ok(pid) = u32::try_from(pid) else {
            return Relation::Unresolvable;
        };
        self.host.resolve(pid, pidversion, children)
    }
}

#[cfg(target_os = "macos")]
#[allow(
    clippy::disallowed_types,
    reason = "the native dead-process control must own and wait for its helper child"
)]
fn assert_real_macos_dead_and_version_mismatch_are_unresolvable() {
    use dudes_rpc::auth::PeerPid;
    use std::io::Write as _;

    let host = RealHost::new();
    let processes = host.peer_processes();

    let (one, _two) = UnixStream::pair().expect("identity pair");
    let mut mismatch = dudes_rpc::peer::identity(&one);
    let PeerPid::Reported {
        pidversion: Some(pidversion),
        ..
    } = &mut mismatch.pid
    else {
        panic!("macOS LOCAL_PEERTOKEN carries pidversion")
    };
    *pidversion = pidversion.wrapping_add(1);
    dispatch_negative_with_resolver(
        "real macOS pidversion mismatch",
        Resolver::processes(Arc::new(CapturedHostPeerProcesses {
            host: HostPeerProcesses(processes.clone()),
            captured: mismatch,
        })),
        NegativeDispatch::Completed,
    );
    dispatch_negative_with_resolver(
        "absent peer identity",
        Resolver::processes(Arc::new(CapturedHostPeerProcesses {
            host: HostPeerProcesses(processes.clone()),
            captured: dudes_rpc::PeerIdentity::unknown(),
        })),
        NegativeDispatch::Completed,
    );

    let dir = tempfile::tempdir().expect("dead-peer directory");
    let socket = dir.path().join("peer.sock");
    let listener = UnixListener::bind(&socket).expect("dead-peer listener");
    let mut child =
        std::process::Command::new(std::env::current_exe().expect("current test executable"))
            .args(["--exact", "the_helper_entry_point", "--quiet"])
            .env(ROLE, DEAD_PEER)
            .env(PEER_SOCKET, &socket)
            .spawn()
            .expect("spawn dead-peer helper");
    let (mut stream, _) = listener.accept().expect("accept dead-peer helper");
    let dead = dudes_rpc::peer::identity(&stream);
    stream.write_all(&[0]).expect("release dead peer");
    assert!(child.wait().expect("wait dead peer").success());
    dispatch_negative_with_resolver(
        "real macOS dead starting execution",
        Resolver::processes(Arc::new(CapturedHostPeerProcesses {
            host: HostPeerProcesses(processes),
            captured: dead,
        })),
        NegativeDispatch::Completed,
    );
}

#[test]
fn sibling_escape_unreadable_and_absent_identity_are_refused() {
    use dudes_rpc::auth::PeerResolution;
    use dudes_rpc::{PeerProvenance, PeerResolver as _};

    let fixture = Fixture::new();
    let release = fixture.state.path().join("release-park");
    let settings = duded::settings::Settings {
        daemon_env: daemon_env(PARK, &release),
        ..fixture.settings()
    };
    let children = ChildEnds::new();
    let host = RealHost::watching(Arc::new(HostStreams(Arc::clone(&children))));
    let resolver = Resolver::processes(Arc::new(HostPeerProcesses(host.peer_processes())));
    let direct = resolver.clone();
    let host = PeerBarrier::new(host, resolver.clone());
    let seams = Seams {
        host,
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(ScriptedRunner::scripted([common::passes()])),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
        resolver,
        adapters: [Box::new(HelperAdapter) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children,
        witness: Witness::default(),
    };
    let tokens = seams.rng.tokens();
    let mut running = common::start(boot(&settings, seams).expect("boot production resolver"));
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    fixture.await_record("TurnSpawned", |record| {
        matches!(record, Record::TurnSpawned { agent: who, .. } if *who == agent).then_some(())
    });
    let token = mint_for(&fixture, &tokens, agent);

    let params = serde_json::to_value(AskParams {
        token,
        text: "sibling must not ask".to_owned(),
    })
    .expect("typed sibling params");
    let mut sibling = dudes_rpc::Client::connect(settings.socket()).expect("sibling connects");
    let response = sibling
        .call(Method::AgentAsk, Some(params))
        .expect("sibling receives refusal");
    assert_eq!(
        response.error.as_ref().map(|error| error.code),
        Some(ErrorCode::Unauthorized.code())
    );
    assert!(
        questions(&fixture).is_empty(),
        "a sibling has zero question effect"
    );

    assert_eq!(
        direct.resolve(&dudes_rpc::PeerIdentity::unknown()),
        PeerResolution::Resolved(PeerProvenance::Unresolvable)
    );

    std::fs::write(&release, b"release").expect("release parked managed child");
    fixture.await_record("TurnEnded", |record| {
        matches!(record, Record::TurnEnded { agent: who, .. } if *who == agent).then_some(())
    });
    assert_eq!(running.stop(ExitCause::Ended), ExitCause::Ended);

    for label in ["escaped peer", "full adoption-key mismatch", "parent cycle"] {
        dispatch_matrix_negative(label, Relation::NoMatch, true);
    }
    dispatch_matrix_negative("unreadable or dead start", Relation::Unresolvable, true);
    #[cfg(target_os = "macos")]
    {
        assert_real_macos_dead_and_version_mismatch_are_unresolvable();
        assert!(
            questions(&fixture).is_empty(),
            "real macOS dead/version mismatch controls have zero HumanQuestion effect"
        );
    }
}

#[test]
fn production_selects_r4_on_linux_macos_and_nowalk_elsewhere() {
    let main = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs"))
        .expect("read production composition");
    assert!(
        target_selection_is_sound(&main),
        "production target selection"
    );

    let compact = compact_source(&main);
    let no_walk_on_supported = compact.replace(SUPPORTED_CONSTRUCTION, SUPPORTED_NO_WALK_MUTANT);
    assert!(
        !target_selection_is_sound(&no_walk_on_supported),
        "Linux/macOS falling back to NoWalk must fail the predicate"
    );

    let walking_unsupported = compact.replace(UNSUPPORTED_CONSTRUCTION, UNSUPPORTED_WALK_MUTANT);
    assert!(
        !target_selection_is_sound(&walking_unsupported),
        "an unsupported target receiving the walking resolver must fail the predicate"
    );

    let widened = compact.replace(
        "target_os=\"macos\"))]",
        "target_os=\"macos\",target_os=\"freebsd\"))]",
    );
    assert!(
        !target_selection_is_sound(&widened),
        "adding an unsupported target to both cfgs must fail the predicate"
    );
}

const SUPPORTED_CONSTRUCTION: &str = "#[cfg(any(windows,target_os=\"linux\",target_os=\"macos\"))]letresolver=duded::classify::Resolver::processes(Arc::new(HostPeerProcesses(host.peer_processes())));";
const UNSUPPORTED_CONSTRUCTION: &str = "#[cfg(not(any(windows,target_os=\"linux\",target_os=\"macos\")))]letresolver=duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk));";
const SUPPORTED_NO_WALK_MUTANT: &str = "#[cfg(any(windows,target_os=\"linux\",target_os=\"macos\"))]letresolver=duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk));";
const UNSUPPORTED_WALK_MUTANT: &str = "#[cfg(not(any(windows,target_os=\"linux\",target_os=\"macos\")))]letresolver=duded::classify::Resolver::processes(Arc::new(HostPeerProcesses(host.peer_processes())));";

fn compact_source(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .flat_map(str::chars)
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn target_selection_is_sound(source: &str) -> bool {
    let compact = compact_source(source);
    compact.matches(SUPPORTED_CONSTRUCTION).count() == 1
        && compact.matches(UNSUPPORTED_CONSTRUCTION).count() == 1
        && compact.matches(SUPPORTED_NO_WALK_MUTANT).count() == 0
        && compact.matches(UNSUPPORTED_WALK_MUTANT).count() == 0
}
