//! The authenticated Floor action wire — onboarding, answers, pause and resume, and goal change —
//! and its journal-backed effects.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use duded::config::load_operator;
use duded::floor_actions::{Enrollment, Profile};
use duded::ports::{Answered, Ask};
use duded::runtime::{OnboardingFailurePoint, OnboardingPausePoint};
use duded::settings::Seams;
use duded::settings::{FloorControl, FloorSettings};
use duded::startup::Witness;
use duded::{ChildEnds, ChildTurn, ExitCause, boot};
use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_proto::ExitStatus;
use dudes_proto::record::{
    BrowserGoal, BrowserGoalKind, BrowserOnboardingGoal, BrowserOnboardingHire, BrowserRetire,
    Hex32, RecordKind,
};
use dudes_proto::rpc::agent::RetireParams;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::floor::SessionIssueResult;
use dudes_proto::rpc::goal::GoalSetParams;
use dudes_proto::rpc::{CurrentMethod, Method};
use dudes_proto::{
    AgentBudget, BudgetScope, Goal, GoalBudget, LoopDecision, Predicate, Record, Spend, TurnOutcome,
};
use dudes_providers::adapter::{
    Adapter, AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
};
use dudes_providers::claude::stream::TurnStream;
use dudes_providers::claude::{ClaudeAdapter, SESSION_ACQUISITION};
use dudes_providers::codex::CodexAdapter;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_providers::pi::PiAdapter;
use dudes_providers::registry::Adapters;
use dudes_providers::stream::{ScopedUsage, TurnReader};
use dudes_rpc::Client;
use dudes_supervisor::Input;
use dudes_supervisor::testing::FakeRng;
use dudes_types::{AgentId, GoalId, IsolationMode, Lsn, Tokens, TurnId, Usage, Usd};
use dudes_types::{ModelSelection, Provider, Tier};
use serde_json::{Value, json};

mod common;
use common::{
    Fixture, ScriptedRunner, SharedHost, TestClock, seams, seams_with, system32_program,
    true_program,
};
use dudes_proto::PeerProvenance;
use dudes_rpc::auth::PeerResolver;

#[test]
fn predicate_path_uses_the_system_root_and_survives_fixture_escaping() {
    let root = tempfile::tempdir().expect("fixture root");
    let system_root = root.path().join("system root's folder");
    let program = system32_program(&system_root, "whoami.exe");
    assert_eq!(program, system_root.join("System32").join("whoami.exe"));
    let encoded = serde_json::to_string(&program.to_str().expect("UTF-8 path"))
        .expect("predicate TOML string");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("repository marker");
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        reload_config(&[("repo_1", &repo)]).replace(
            &serde_json::to_string(true_program()).expect("predicate TOML string"),
            &encoded,
        ),
    )
    .expect("escaped predicate fixture");
    let settings = load_operator(&config).expect("escaped predicate loads");
    let Predicate::Command { argv, .. } = &settings.enrollment.profiles[0].checks[0].predicate
    else {
        panic!("expected command predicate");
    };
    assert_eq!(argv, &[program.to_str().expect("UTF-8 path")]);
}

const HELPER_ROLE: &str = "DUDES_FLOOR_ONBOARDING_HELPER";
/// When set, the stub child holds its turn open after its first line until this path exists, for
/// at most thirty seconds, so a test can act while a Managed turn is running.
const STUB_HOLD: &str = "DUDES_FLOOR_STUB_HOLD";
/// When set, the stub child reports the dollar cost this file holds as its turn ends, instead of
/// $0.10, so a test can price each turn.
const STUB_COST: &str = "DUDES_FLOOR_STUB_COST";

struct LogCapture(Arc<Mutex<Vec<String>>>);

struct LogFields(String);

impl tracing::field::Visit for LogFields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        let _ = write!(&mut self.0, "{}={value:?} ", field.name());
    }
}

impl tracing::Subscriber for LogCapture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = LogFields(String::new());
        event.record(&mut fields);
        self.0.lock().expect("log capture").push(fields.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn daemon_owned_managed_child() {
    use std::io::Write as _;
    if std::env::var_os(HELPER_ROLE).is_none() {
        return;
    }
    // A task delivered at launch arrives here on standard input; the answer tests read it back.
    let mut task = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut task);
    if let Some(dir) = std::env::var_os(TASK_CAPTURE) {
        let _ = std::fs::write(
            PathBuf::from(dir).join(format!("{}.task", std::process::id())),
            &task,
        );
    }
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let _ = output.write_all(br#"{"type":"system","subtype":"init","session_id":"3f9a1c2e-0000-4000-8000-0000000012ab","tools":[]}
"#);
    if let Some(hold) = std::env::var_os(STUB_HOLD).map(PathBuf::from) {
        let _ = output.flush();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !hold.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let cost = std::env::var_os(STUB_COST)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map_or_else(|| "0.1".to_owned(), |cost| cost.trim().to_owned());
    let _ = writeln!(
        output,
        r#"{{"type":"result","subtype":"success","is_error":false,"session_id":"3f9a1c2e-0000-4000-8000-0000000012ab","result":"stub finished","total_cost_usd":{cost}}}"#
    );
    let _ = output.flush();
    std::process::exit(0);
}

#[derive(Debug)]
struct StubAdapter;

impl Adapter for StubAdapter {
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
        task: &str,
        _session: &TurnSession,
        _selection: &ModelSelection,
    ) -> Result<LaunchSpec, dudes_providers::launch::LaunchError> {
        Ok(LaunchSpec {
            argv: vec![
                OsString::from(std::env::current_exe().expect("test binary")),
                OsString::from("--exact"),
                OsString::from("daemon_owned_managed_child"),
                OsString::from("--quiet"),
            ],
            env: ctx.daemon_env.to_vec(),
            cwd: ctx.cwd.to_path_buf(),
            // Delivered once at launch and closed, as a managed task may be (invariant 6).
            stdin: dudes_providers::launch::LaunchInput::WriteThenClose(task.as_bytes().to_vec()),
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
    fn watch(&self, key: Option<&dudes_supervisor::host::AdoptionKey>) -> Box<dyn ChildStream> {
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

fn real_stub_seams(
    settings: &duded::Settings,
) -> Seams<RealHost, ScriptedRunner, TestClock, common::FixtureRng> {
    real_stub_seams_with(settings, ScriptedRunner::scripted([common::passes()]))
}

fn real_stub_seams_with(
    settings: &duded::Settings,
    predicates: ScriptedRunner,
) -> Seams<RealHost, ScriptedRunner, TestClock, common::FixtureRng> {
    let children = ChildEnds::new();
    Seams {
        host: RealHost::watching(Arc::new(HostStreams(Arc::clone(&children)))),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(predicates),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(settings, FakeRng::new()),
        resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
        adapters: [Box::new(StubAdapter) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children,
        witness: Witness::default(),
    }
}

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
);

fn enrolled_settings(fixture: &Fixture) -> duded::Settings {
    let repo = fixture.state.path().join("enrolled-repo");
    std::fs::create_dir_all(repo.join(".git")).expect("repository marker");
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.enrollment = Enrollment {
        profiles: vec![Profile {
            id: "repo_1".into(),
            label: "Service workspace".into(),
            checks: vec![passing_check()],
            cwd: repo.canonicalize().expect("canonical repository"),
            written_cwd: repo.canonicalize().expect("canonical repository"),
            department: "engineering".into(),
            isolation: IsolationMode::Default,
            agent_budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            goal_budget: GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
            hop_cap: 3,
            mark: None,
        }],
    };
    settings
}

#[test]
fn operator_catalog_rejects_count_and_independent_serialized_size_limits() {
    let program = serde_json::to_string(true_program()).expect("predicate TOML string");
    let root = tempfile::tempdir().expect("operator config root");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("repository marker");
    let config = root.path().join("config.toml");
    let make = |count: usize, large: bool| {
        let mut source = String::from(
            "[budget]\nfloor_usd_per_day = 100\n[budget.per_turn_floor]\nusd = 0.000001\n",
        );
        for i in 0..count {
            let id = if large {
                format!("repo_{i:02}_{}", "a".repeat(120))
            } else {
                format!("repo_{i:02}")
            };
            let label = if large {
                "\"".repeat(128)
            } else {
                "Repository".into()
            };
            let check = if large {
                "\"".repeat(128)
            } else {
                "Checks pass".into()
            };
            let _ = write!(
                source,
                "[[floor.onboarding.workspaces]]\nworkspace_id = '{id}'\nlabel = '{label}'\ncompletion_check_label = '{check}'\ncwd = '{}'\ndepartment = 'engineering'\nisolation = 'default'\nagent_usd_per_day = 10\ngoal_usd = 5\nhop_cap = 3\npredicate_argv = [{program}]\npredicate_timeout_s = 10\n",
                repo.display(),
            );
        }
        source
    };
    std::fs::write(&config, make(32, false)).expect("valid max-count catalog");
    assert_eq!(
        load_operator(&config)
            .expect("32 small profiles")
            .enrollment
            .profiles
            .len(),
        32
    );
    std::fs::write(&config, make(33, false)).expect("over-count catalog");
    assert!(load_operator(&config).is_err(), "33 profiles are refused");
    std::fs::write(&config, make(32, true)).expect("over-size catalog");
    assert!(
        load_operator(&config).is_err(),
        "32 individually valid profiles can independently exceed the 16 KiB escaped JSON cap"
    );
    std::fs::write(
        &config,
        make(1, false).replace("isolation = 'default'", "isolation = 'inherit'"),
    )
    .expect("unsupported per-agent isolation");
    assert!(
        load_operator(&config).is_err(),
        "only the configured default isolation is executable"
    );
}

#[test]
fn operator_catalog_display_bounds_and_controls_refuse_before_exposure() {
    let program = serde_json::to_string(true_program()).expect("predicate TOML string");
    let root = tempfile::tempdir().expect("operator config root");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("repository marker");
    let config = root.path().join("config.toml");
    let source = |label: &str, check: &str| {
        format!(
            "[budget]\nfloor_usd_per_day = 100\n[budget.per_turn_floor]\nusd = 0.000001\n[[floor.onboarding.workspaces]]\nworkspace_id = 'repo_1'\nlabel = {label}\ncompletion_check_label = {check}\ncwd = '{}'\ndepartment = 'engineering'\nisolation = 'default'\nagent_usd_per_day = 10\ngoal_usd = 5\nhop_cap = 3\npredicate_argv = [{program}]\npredicate_timeout_s = 10\n",
            repo.display(),
        )
    };
    let max_label = format!("'{}'", "X".repeat(128));
    std::fs::write(&config, source(&max_label, &max_label)).expect("max display strings");
    assert!(load_operator(&config).is_ok(), "128 UTF-8 bytes are valid");
    let over_label = format!("'{}'", "X".repeat(129));
    for (label, check) in [
        (over_label.as_str(), max_label.as_str()),
        (max_label.as_str(), over_label.as_str()),
        (r#""bad\u0001""#, max_label.as_str()),
        (max_label.as_str(), r#""bad\u0001""#),
    ] {
        std::fs::write(&config, source(label, check)).expect("invalid display config");
        assert!(
            load_operator(&config).is_err(),
            "invalid label/check is refused: {label:?} {check:?}"
        );
    }
}

#[track_caller]
fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        if let Err(error) = stream.read_exact(&mut byte) {
            let caller = std::panic::Location::caller();
            panic!(
                "incomplete response head: {} bytes acquired at {}:{} ({:?})",
                head.len(),
                caller.file(),
                caller.line(),
                error.kind()
            );
        }
        head.push(byte[0]);
    }
    String::from_utf8(head).expect("ASCII head")
}

fn issue(settings: &duded::Settings) -> SessionIssueResult {
    let credential = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("operator credential");
    let mut client = Client::connect(settings.socket()).expect("daemon socket");
    let reply = client
        .call_as_operator(
            Method::FloorSessionIssue,
            None,
            OperatorCredential::new(credential).expect("credential"),
        )
        .expect("issue response");
    serde_json::from_value(reply.result.expect("issue succeeded")).expect("typed issue")
}

fn exchange(addr: SocketAddr, bootstrap: &str) -> (String, String) {
    let host = addr.to_string();
    let mut stream = TcpStream::connect(addr).expect("Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    let request = format!(
        "POST /floor/session HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n\r\n{bootstrap}",
        bootstrap.len()
    );
    stream.write_all(request.as_bytes()).expect("exchange");
    let head = read_head(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    let cookie = head
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: dudes_floor_session="))
        .and_then(|value| value.split(';').next())
        .expect("cookie")
        .to_owned();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .expect("length")
        .parse()
        .expect("decimal length");
    let mut body = vec![0; length];
    stream.read_exact(&mut body).expect("exchange body");
    let csrf = serde_json::from_slice::<Value>(&body).expect("JSON")["csrf"]
        .as_str()
        .expect("csrf")
        .to_owned();
    (cookie, csrf)
}

fn snapshot(addr: SocketAddr, cookie: &str, csrf: &str) -> (Value, Vec<u8>) {
    snapshot_optional(addr, cookie, csrf).expect("authenticated Floor snapshot")
}

fn snapshot_optional(addr: SocketAddr, cookie: &str, csrf: &str) -> Option<(Value, Vec<u8>)> {
    snapshot_optional_signaled(addr, cookie, csrf, None)
}

fn snapshot_optional_signaled(
    addr: SocketAddr,
    cookie: &str,
    csrf: &str,
    sent: Option<std::sync::mpsc::Sender<()>>,
) -> Option<(Value, Vec<u8>)> {
    let host = addr.to_string();
    let mut stream = TcpStream::connect(addr).expect("Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    let request = format!(
        "GET /floor/stream HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: dudes-floor-v3, dudes-csrf.{csrf}\r\nCookie: dudes_floor_session={cookie}\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).expect("upgrade");
    assert!(read_head(&mut stream).starts_with("HTTP/1.1 101 Switching Protocols"));
    let subscribe = br#"{"type":"floor.subscribe"}"#;
    let mut frame = vec![
        0x81,
        0x80 | u8::try_from(subscribe.len()).expect("small frame"),
    ];
    let mask = [1_u8, 2, 3, 4];
    frame.extend_from_slice(&mask);
    frame.extend(
        subscribe
            .iter()
            .enumerate()
            .map(|(i, byte)| byte ^ mask[i % 4]),
    );
    stream.write_all(&frame).expect("subscribe");
    if let Some(sent) = sent {
        sent.send(()).expect("subscription sent signal");
    }
    let mut head = [0; 2];
    match stream.read_exact(&mut head) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            return None;
        }
        Err(error) => panic!("snapshot frame: {error}"),
    }
    if head[0] == 0x88 {
        return None;
    }
    assert_eq!(head[0], 0x81);
    let length = match head[1] & 0x7f {
        n @ 0..=125 => usize::from(n),
        126 => {
            let mut bytes = [0; 2];
            stream.read_exact(&mut bytes).expect("u16 length");
            usize::from(u16::from_be_bytes(bytes))
        }
        127 => {
            let mut bytes = [0; 8];
            stream.read_exact(&mut bytes).expect("u64 length");
            usize::try_from(u64::from_be_bytes(bytes)).expect("bounded snapshot")
        }
        _ => unreachable!(),
    };
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).expect("snapshot payload");
    Some((
        serde_json::from_slice(&payload).expect("snapshot JSON"),
        payload,
    ))
}

fn upgrade_head(addr: SocketAddr, host: &str, origin: &str, cookie: &str, csrf: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    let request = format!(
        "GET /floor/stream HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: dudes-floor-v3, dudes-csrf.{csrf}\r\nCookie: dudes_floor_session={cookie}\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .expect("upgrade request");
    read_head(&mut stream)
}

/// The fixtures' enrolled check: one the operator declared may already pass, so a hire does not
/// run it first. A test about that run enrolls its own.
fn passing_check() -> duded::floor_actions::EnrolledCheck {
    duded::floor_actions::EnrolledCheck {
        may_already_pass: true,
        ..duded::floor_actions::EnrolledCheck::enrolled(
            "Checks pass",
            Predicate::Command {
                argv: vec![true_program().into()],
                timeout_s: 10,
            },
        )
    }
}

/// A new hire names the fixtures' enrolled check, as the page does from the `checks` sibling. A
/// body whose `check_id` is null is sent without the check, as a retry of an older attempt is.
fn naming_the_check(body: &Value) -> Value {
    let mut body = body.clone();
    if let Some(fields) = body.as_object_mut()
        && fields.get("check_id") == Some(&Value::Null)
    {
        fields.remove("check_id");
        fields.remove("check_revision");
    } else if let Some(fields) = body.as_object_mut()
        && fields.contains_key("workspace_id")
        && !fields.contains_key("check_id")
    {
        fields.insert("check_id".into(), json!("default"));
        fields.insert(
            "check_revision".into(),
            json!(common::check_revision_as(
                "default",
                "Checks pass",
                &[true_program()],
                10,
                true
            )),
        );
    }
    body
}

#[track_caller]
fn post_onboard(addr: SocketAddr, cookie: &str, csrf: &str, body: &Value) -> (String, Value) {
    post_onboard_signaled(addr, cookie, csrf, body, None)
}

#[track_caller]
fn post_onboard_signaled(
    addr: SocketAddr,
    cookie: &str,
    csrf: &str,
    body: &Value,
    sent: Option<std::sync::mpsc::Sender<()>>,
) -> (String, Value) {
    let host = addr.to_string();
    let body = serde_json::to_vec(&naming_the_check(body)).expect("onboarding request JSON");
    let mut stream = TcpStream::connect(addr).expect("Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let head = format!(
        "POST /floor/onboard HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: dudes_floor_session={cookie}\r\nX-Dudes-CSRF: {csrf}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("request head");
    stream.write_all(&body).expect("request body");
    if let Some(sent) = sent {
        sent.send(()).expect("request sent signal");
    }
    let head = read_head(&mut stream);
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .expect("response length")
        .parse()
        .expect("decimal length");
    let mut body = vec![0; length];
    stream.read_exact(&mut body).expect("response body");
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).expect("response JSON")
    };
    (head, value)
}

fn raw_onboard(addr: SocketAddr, request: &[u8]) -> (String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).expect("Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("timeout");
    stream.write_all(request).expect("raw request");
    let head = read_head(&mut stream);
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .expect("response length")
        .parse()
        .expect("decimal length");
    let mut body = vec![0; length];
    stream.read_exact(&mut body).expect("response body");
    (head, body)
}

#[test]
fn browser_enrollment_view_is_finite_and_safe() {
    let fixture = Fixture::new();
    let repo = fixture.state.path().join("enrolled-repo");
    std::fs::create_dir_all(repo.join(".git")).expect("repository marker");
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.enrollment = Enrollment {
        profiles: vec![Profile {
            id: "repo_1".into(),
            label: "Service workspace".into(),
            checks: vec![passing_check()],
            cwd: repo.canonicalize().expect("canonical repository"),
            written_cwd: repo.canonicalize().expect("canonical repository"),
            department: "engineering".into(),
            isolation: IsolationMode::Default,
            agent_budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            goal_budget: GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
            hop_cap: 3,
            mark: None,
        }],
    };
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
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (frame, bytes) = snapshot(addr, &cookie, &csrf);
    assert_eq!(frame["type"], "floor.snapshot");
    assert!(frame["projection"].is_object());
    assert_eq!(
        frame["onboarding"],
        json!({
            "floor_usd_per_day_micros":"100000000",
            "workspaces":[{
                "workspace_id":"repo_1", "label":"Service workspace", "engines":["claude"],
                "goal_usd_micros":"5000000",
                "agent_usd_per_day_micros":"10000000", "agent_tokens_per_day":null,
                "goal_tokens":null, "hop_cap":3
            }]
        })
    );
    assert!(
        !bytes
            .windows(repo.to_string_lossy().len())
            .any(|window| window == repo.to_string_lossy().as_bytes())
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn enrolled_snapshot_refuses_wrong_host_origin_cookie_and_csrf() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
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
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let host = addr.to_string();
    let origin = format!("http://{host}");
    assert!(
        snapshot(addr, &cookie, &csrf).0["onboarding"].is_object(),
        "positive control: this session has a catalog"
    );
    for (label, candidate_host, candidate_origin, candidate_cookie, candidate_csrf) in [
        (
            "Host",
            "invalid",
            origin.as_str(),
            cookie.as_str(),
            csrf.as_str(),
        ),
        (
            "Origin",
            host.as_str(),
            "null",
            cookie.as_str(),
            csrf.as_str(),
        ),
        (
            "cookie",
            host.as_str(),
            origin.as_str(),
            "invalid",
            csrf.as_str(),
        ),
        (
            "CSRF",
            host.as_str(),
            origin.as_str(),
            cookie.as_str(),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ),
    ] {
        let head = upgrade_head(
            addr,
            candidate_host,
            candidate_origin,
            candidate_cookie,
            candidate_csrf,
        );
        assert!(
            head.starts_with("HTTP/1.1 403 Forbidden"),
            "{label}: {head}"
        );
        assert!(
            !head.contains("onboarding"),
            "{label}: no catalog in refusal"
        );
        assert!(
            head.contains("Cache-Control: no-store"),
            "{label}: no cache"
        );
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn catalog_sentinels_stay_out_of_frame_asset_response_url_and_debug_log_values() {
    let logs = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::set_global_default(LogCapture(Arc::clone(&logs)))
        .expect("capture this test binary's real daemon log events");
    let fixture = Fixture::new();
    let mut settings = enrolled_settings(&fixture);
    let repo = fixture.state.path().join("private-cwd-sentinel");
    std::fs::create_dir_all(repo.join(".git")).expect("sentinel repository");
    settings.enrollment.profiles[0].cwd = repo.canonicalize().expect("canonical sentinel");
    settings.enrollment.profiles[0].checks[0].predicate = Predicate::Command {
        argv: vec![true_program().into(), "private-argv-sentinel".into()],
        timeout_s: 10,
    };
    settings
        .daemon_env
        .push(EnvVar::new("PRIVATE_TOKEN", "private-credential-sentinel"));
    let debug_log_value = format!("{settings:?}");
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
    .expect("boot Floor");
    let fault = daemon.onboarding_failure();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (frame, frame_bytes) = snapshot(addr, &cookie, &csrf);
    assert!(
        frame["onboarding"].is_object(),
        "positive control: eligible sentinel profile"
    );
    let host = addr.to_string();
    let static_request =
        format!("GET /floor/release-manifest.json HTTP/1.1\r\nHost: {host}\r\n\r\n");
    let (static_head, static_body) = raw_onboard(addr, static_request.as_bytes());
    assert!(
        static_head.starts_with("HTTP/1.1 200 OK"),
        "static positive control"
    );
    fault.arm(OnboardingFailurePoint::Hire);
    let request = json!({
        "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship",
        "check_id": "default",
        "check_revision": common::check_revision_as(
            "default", "Checks pass", &[true_program(), "private-argv-sentinel"], 10, true
        )
    });
    let (failed_head, failed_body) = post_onboard(addr, &cookie, &csrf, &request);
    assert!(
        failed_head.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{failed_head}"
    );
    assert!(failed_body.is_null());
    let captured_logs = logs.lock().expect("captured daemon logs").join("\n");
    assert!(
        captured_logs.contains("restart to replay before new work or writes"),
        "positive control: the actual daemon failure log was captured"
    );
    let surfaces = [
        (
            "frame",
            String::from_utf8(frame_bytes).expect("snapshot UTF-8"),
        ),
        (
            "static asset",
            String::from_utf8(static_body).expect("manifest UTF-8"),
        ),
        ("URL and headers", format!("{static_request}{static_head}")),
        ("debug/log format", debug_log_value),
        ("captured runtime log", captured_logs),
    ];
    for (surface, text) in surfaces {
        for sentinel in [
            "private-cwd-sentinel",
            "private-argv-sentinel",
            "private-credential-sentinel",
        ] {
            assert!(!text.contains(sentinel), "{surface} exposed {sentinel}");
        }
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_catalog_preserves_micro_dollars_above_javascript_safe_integer() {
    let fixture = Fixture::new();
    let mut settings = enrolled_settings(&fixture);
    let large = Usd::from_micros(9_007_199_254_740_993);
    settings.floor_usd_per_day = Some(large);
    settings.enrollment.profiles[0].agent_budget.usd_per_day = Some(large);
    settings.enrollment.profiles[0].goal_budget.usd = Some(large);
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
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (frame, _) = snapshot(addr, &cookie, &csrf);
    for value in [
        "floor_usd_per_day_micros",
        "agent_usd_per_day_micros",
        "goal_usd_micros",
    ] {
        let actual = if value == "floor_usd_per_day_micros" {
            &frame["onboarding"][value]
        } else {
            &frame["onboarding"]["workspaces"][0][value]
        };
        assert_eq!(actual, "9007199254740993", "{value} stays decimal text");
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn fresh_snapshot_omits_disabled_or_drifted_enrollment() {
    let fixture = Fixture::new();
    for case in [
        "no profiles",
        "hire off",
        "no adapter",
        "unsupported isolation",
    ] {
        let mut settings = enrolled_settings(&fixture);
        match case {
            "no profiles" => settings.enrollment.profiles.clear(),
            "hire off" => settings.floor_hire_enabled = false,
            "no adapter" => {}
            "unsupported isolation" => {
                settings.enrollment.profiles[0].isolation = IsolationMode::Inherit;
            }
            _ => unreachable!(),
        }
        let mut configured = seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        );
        if case == "no adapter" {
            configured.adapters = Adapters::new(Vec::<Box<dyn Adapter>>::new());
        }
        let daemon = boot(&settings, configured).expect("boot disabled Floor");
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let (frame, _) = snapshot(addr, &cookie, &csrf);
        assert!(frame.get("onboarding").is_none(), "{case}");
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }

    let settings = enrolled_settings(&fixture);
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
    .expect("boot enrolled Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    std::fs::remove_dir_all(fixture.state.path().join("enrolled-repo/.git"))
        .expect("invalidate repository marker after boot");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (frame, _) = snapshot(addr, &cookie, &csrf);
    assert!(
        frame.get("onboarding").is_none(),
        "drifted workspace is not advertised"
    );
    let request = json!({
        "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
    });
    let (head, body) = post_onboard(addr, &cookie, &csrf, &request);
    assert!(head.starts_with("HTTP/1.1 403 Forbidden"), "{head}");
    assert!(body.is_null());
    assert!(!fixture.records().iter().any(|record| matches!(
        record,
        Record::AgentHired {
            browser_onboarding: Some(_),
            ..
        }
    )));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// `enrolled_settings` with no budget anywhere: no floor ceiling, the loader's default reserves,
/// and a workspace with no ceiling of its own.
fn uncapped_settings(fixture: &Fixture) -> duded::Settings {
    let mut settings = enrolled_settings(fixture);
    settings.floor_usd_per_day = None;
    settings.per_turn_floor = Spend::DEFAULT_PER_TURN_FLOOR;
    let profile = &mut settings.enrollment.profiles[0];
    profile.agent_budget = AgentBudget {
        usd_per_day: None,
        tokens_per_day: None,
    };
    profile.goal_budget = GoalBudget {
        usd: None,
        tokens: None,
    };
    settings
}

/// A signed-in Floor over `settings` on a daemon whose registry holds Claude, Codex and Pi.
fn every_engine_floor(settings: &duded::Settings) -> (common::Running, SocketAddr, String, String) {
    let daemon = boot(
        settings,
        Seams {
            adapters: Adapters::new([
                Box::new(ClaudeAdapter::default()) as Box<dyn Adapter>,
                Box::new(CodexAdapter::default()),
                Box::new(PiAdapter::default()),
            ]),
            ..seams(
                settings,
                SharedHost::new(),
                Arc::new(ScriptedRunner::default()),
                Arc::new(TestClock::new()),
                Witness::default(),
            )
        },
    )
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(settings).bootstrap);
    (running, addr, cookie, csrf)
}

/// A browser hire of `repo_1` under the attempt id that starts with `attempt`, naming `provider`
/// when there is one.
fn hire_request(attempt: char, provider: Option<&str>) -> Value {
    let mut request = json!({
        "attempt_id": format!("{attempt}{}", "A".repeat(42)),
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
    });
    if let Some(provider) = provider {
        request["provider"] = json!(provider);
    }
    request
}

/// Each browser hire's engine and isolation, in journal order.
fn browser_hires(fixture: &Fixture) -> Vec<(Provider, Option<IsolationMode>)> {
    fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::AgentHired {
                provider,
                isolation,
                browser_onboarding: Some(_),
                ..
            } => Some((provider, isolation)),
            _ => None,
        })
        .collect()
}

/// The binding ceiling of the first refused `BudgetDecision`, once one is on the disk.
fn await_refused_ceiling(fixture: &Fixture) -> dudes_proto::Ceiling {
    fixture.await_record("a refused BudgetDecision", |record| match record {
        Record::BudgetDecision {
            allowed: false,
            ceiling,
            ..
        } => Some(*ceiling),
        _ => None,
    })
}

fn any_turn_issued(fixture: &Fixture) -> bool {
    fixture
        .records()
        .iter()
        .any(|record| matches!(record, Record::TurnIssued { .. }))
}

/// With `[floor.hire]` on and no budget anywhere, the catalog offers every
/// registered engine with each ceiling `null`, the settings sibling reports the switch and the
/// default reserves, and a hire of each engine completes.
#[test]
fn uncapped_hires_of_every_engine_need_no_budget() {
    let fixture = Fixture::new();
    let settings = uncapped_settings(&fixture);
    let (mut running, addr, cookie, csrf) = every_engine_floor(&settings);
    let frame = snapshot(addr, &cookie, &csrf).0;
    assert_eq!(
        frame["onboarding"],
        json!({
            "floor_usd_per_day_micros": null,
            "workspaces": [{
                "workspace_id":"repo_1", "label":"Service workspace",
                "engines":["claude", "codex", "pi"], "goal_usd_micros":null, "agent_usd_per_day_micros":null,
                "agent_tokens_per_day":null, "goal_tokens":null, "hop_cap":3
            }]
        })
    );
    assert_eq!(frame["settings"]["switches"]["hire"], true);
    assert_eq!(
        frame["settings"]["budget"],
        json!({
            "floor_usd_per_day_micros": null, "per_turn_usd_micros": "250000",
            "per_turn_tokens": "20000"
        })
    );
    for (attempt, provider) in [('A', None), ('B', Some("codex")), ('C', Some("pi"))] {
        let (head, response) = post_onboard(addr, &cookie, &csrf, &hire_request(attempt, provider));
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{provider:?}: {head}");
        assert_eq!(response["state"], "complete", "{provider:?}");
    }
    assert_eq!(
        browser_hires(&fixture)
            .into_iter()
            .map(|(provider, _)| provider)
            .collect::<Vec<_>>(),
        [Provider::Claude, Provider::Codex, Provider::Pi]
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// `AgentHired` records the engine the browser chose, with the workspace's
/// isolation for Claude and none for Codex and Pi, as `agent.hire` requires of them.
#[test]
fn browser_hires_of_codex_and_pi_record_the_engine_without_isolation() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
    let (mut running, addr, cookie, csrf) = every_engine_floor(&settings);
    for (attempt, provider) in [('A', Some("codex")), ('B', Some("pi")), ('C', None)] {
        let (head, _) = post_onboard(addr, &cookie, &csrf, &hire_request(attempt, provider));
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{provider:?}: {head}");
    }
    assert_eq!(
        browser_hires(&fixture),
        [
            (Provider::Codex, None),
            (Provider::Pi, None),
            (Provider::Claude, Some(IsolationMode::Default)),
        ]
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// An engine the daemon does not offer for the workspace, here because this run registered no
/// Codex or Pi adapter, is the uniform no-effect `403`, with nothing appended. The offered
/// engine is the control.
#[test]
fn an_engine_that_is_not_offered_is_refused_before_any_append() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
    let (_, mut running, addr, cookie, csrf) = default_floor(&settings);
    assert_eq!(
        snapshot(addr, &cookie, &csrf).0["onboarding"]["workspaces"][0]["engines"],
        json!(["claude"])
    );
    let before = fixture.records();
    for (attempt, provider) in [('A', "codex"), ('B', "pi")] {
        let (head, body) =
            post_onboard(addr, &cookie, &csrf, &hire_request(attempt, Some(provider)));
        assert!(
            head.starts_with("HTTP/1.1 403 Forbidden"),
            "{provider}: {head}"
        );
        assert!(body.is_null(), "{provider}");
    }
    assert_eq!(
        fixture.records(),
        before,
        "a refused engine appends nothing"
    );
    let (head, _) = post_onboard(addr, &cookie, &csrf, &hire_request('C', None));
    assert!(head.starts_with("HTTP/1.1 200 OK"), "control: {head}");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The request digest covers the engine, so a same-attempt retry that names another engine, or
/// none, is an empty `409` with no second hire. The unchanged retry answers the original ids.
#[test]
fn a_retry_that_changes_the_engine_conflicts() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
    let (mut running, addr, cookie, csrf) = every_engine_floor(&settings);
    let codex = hire_request('A', Some("codex"));
    let (head, complete) = post_onboard(addr, &cookie, &csrf, &codex);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    for changed in [hire_request('A', Some("pi")), hire_request('A', None)] {
        let (head, conflict) = post_onboard(addr, &cookie, &csrf, &changed);
        assert!(
            head.starts_with("HTTP/1.1 409 Conflict"),
            "{changed}: {head}"
        );
        assert!(conflict.is_null(), "{changed}");
    }
    let (_, retry) = post_onboard(addr, &cookie, &csrf, &codex);
    assert_eq!(retry, complete);
    assert_eq!(browser_hires(&fixture), [(Provider::Codex, None)]);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// At the browser: with Codex's billing variable in the daemon's environment, a
/// workspace whose ceilings are all in dollars does not offer Codex, and a crafted Codex hire is
/// the no-effect `403`. Claude and Pi, whose variables are absent, stay offered. An agent token
/// ceiling counts for Codex, and the same hire is admitted.
#[test]
fn a_billing_key_withdraws_codex_until_a_token_ceiling_counts() {
    for tokens in [None, Some(Tokens::new(60_000))] {
        let fixture = Fixture::new();
        let mut settings = enrolled_settings(&fixture);
        settings.daemon_env.push(EnvVar::new(
            "CODEX_API_KEY",
            "fixture-value-not-a-credential",
        ));
        settings.enrollment.profiles[0].agent_budget.tokens_per_day = tokens;
        let (mut running, addr, cookie, csrf) = every_engine_floor(&settings);
        let engines =
            snapshot(addr, &cookie, &csrf).0["onboarding"]["workspaces"][0]["engines"].clone();
        let (head, _) = post_onboard(addr, &cookie, &csrf, &hire_request('A', Some("codex")));
        if tokens.is_none() {
            assert_eq!(engines, json!(["claude", "pi"]));
            assert!(head.starts_with("HTTP/1.1 403 Forbidden"), "{head}");
            assert_eq!(browser_hires(&fixture), []);
        } else {
            assert_eq!(engines, json!(["claude", "codex", "pi"]));
            assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
            assert_eq!(browser_hires(&fixture), [(Provider::Codex, None)]);
        }
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

/// With `[floor.hire]` off, no session gets the catalog, the settings sibling
/// says so, and a hire is the no-effect `403` though every other condition holds. The switch on is
/// the control.
#[test]
fn browser_hire_is_refused_while_the_hire_switch_is_off() {
    for enabled in [false, true] {
        let fixture = Fixture::new();
        let mut settings = enrolled_settings(&fixture);
        settings.floor_hire_enabled = enabled;
        let (_, mut running, addr, cookie, csrf) = default_floor(&settings);
        let frame = snapshot(addr, &cookie, &csrf).0;
        assert_eq!(frame.get("onboarding").is_some(), enabled);
        assert_eq!(frame["settings"]["switches"]["hire"], enabled);
        let (head, body) = post_onboard(addr, &cookie, &csrf, &hire_request('A', None));
        if enabled {
            assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        } else {
            assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
            assert_eq!(body["reason"], "action_off", "{body}");
        }
        assert_eq!(browser_hires(&fixture).len(), usize::from(enabled));
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

/// Invariant 4 for a browser hire: a ceiling the workspace sets still binds at admission. A goal
/// token ceiling of 0 admits the hire and its goal, and the budget gate refuses the first turn with a
/// journaled `BudgetDecision` in the goal scope. With no ceiling, the turn is issued.
#[test]
fn a_set_ceiling_still_refuses_a_browser_hire_at_admission() {
    for ceiling in [Some(Tokens::ZERO), None] {
        let fixture = Fixture::new();
        let mut settings = uncapped_settings(&fixture);
        settings.enrollment.profiles[0].goal_budget.tokens = ceiling;
        let (_, mut running, addr, cookie, csrf) = default_floor(&settings);
        let (head, response) = post_onboard(addr, &cookie, &csrf, &hire_request('A', None));
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert_eq!(response["state"], "complete");
        if ceiling.is_some() {
            let binding = await_refused_ceiling(&fixture);
            assert_eq!(
                (binding.scope, binding.tokens),
                (BudgetScope::Goal, Some(Tokens::ZERO))
            );
            assert!(!any_turn_issued(&fixture), "no turn at a reached ceiling");
        } else {
            fixture.await_turn();
        }
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

/// A journal written before budgets were optional replays: a partial Claude browser hire recorded
/// with the workspace's dollar ceilings and isolation completes its goal after the upgrade, and the
/// ceilings it recorded still bind that goal's first turn.
#[test]
fn an_old_journal_with_ceilings_replays_and_its_ceilings_still_bind() {
    let fixture = Fixture::new();
    let mut settings = enrolled_settings(&fixture);
    settings.enrollment.profiles[0].goal_budget.tokens = Some(Tokens::ZERO);
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .expect("first boot");
    let pause = daemon.onboarding_pause();
    pause.arm(OnboardingPausePoint::BeforeGoalPermit);
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let request = hire_request('A', None);
    let owner_request = request.clone();
    let owner = std::thread::spawn(move || post_onboard(addr, &cookie, &csrf, &owner_request));
    assert!(
        pause.wait_reached(Duration::from_secs(3)),
        "the hire is durable before the goal"
    );
    clock.advance(8 * 60 * 60 * 1000);
    pause.release();
    let (_, partial) = owner.join().expect("partial response");
    assert_eq!(partial["reason"], "session_expired");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    let recorded = fixture
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::AgentHired {
                provider,
                isolation,
                budget,
                browser_onboarding: Some(_),
                ..
            } => Some((provider, isolation, budget)),
            _ => None,
        })
        .expect("the partial hire");
    assert_eq!(
        recorded,
        (
            Provider::Claude,
            Some(IsolationMode::Default),
            AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            }
        ),
        "the record a Claude hire has always written"
    );

    let (_, mut running, addr, cookie, csrf) = default_floor(&settings);
    let (head, complete) = post_onboard(addr, &cookie, &csrf, &request);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(complete["state"], "complete");
    let binding = await_refused_ceiling(&fixture);
    assert_eq!(
        (binding.scope, binding.usd, binding.tokens),
        (
            BudgetScope::Goal,
            Some(Usd::from_dollars(5)),
            Some(Tokens::ZERO)
        )
    );
    assert!(!any_turn_issued(&fixture), "no turn at a reached ceiling");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end stub scenario asserts the complete durable effect chain"
)]
fn browser_hire_briefing_is_finite_and_attributed() {
    let fixture = Fixture::new();
    let repo = fixture.state.path().join("enrolled-repo");
    std::fs::create_dir_all(repo.join(".git")).expect("repository marker");
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.enrollment = Enrollment {
        profiles: vec![Profile {
            id: "repo_1".into(),
            label: "Service workspace".into(),
            checks: vec![passing_check()],
            cwd: repo.canonicalize().expect("canonical repository"),
            written_cwd: repo.canonicalize().expect("canonical repository"),
            department: "engineering".into(),
            isolation: IsolationMode::Default,
            agent_budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            goal_budget: GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
            hop_cap: 3,
            mark: None,
        }],
    };
    settings.daemon_env.push(EnvVar::new(HELPER_ROLE, "1"));
    let daemon = boot(&settings, real_stub_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let request = json!({
        "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer",
        "goal_text":"ship one feature"
    });
    let (head, response) = post_onboard(addr, &cookie, &csrf, &request);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(head.contains("Cache-Control: no-store"));
    assert!(!head.to_ascii_lowercase().contains("access-control-allow"));
    assert_eq!(response["state"], "complete");
    let agent = response["agent"].as_str().expect("real AgentId");
    let goal = response["goal"].as_str().expect("real GoalId");
    let records = fixture.records();
    let encoded = serde_json::to_string(&fixture.entries()).expect("journal entries encode");
    assert!(
        !encoded.contains(&cookie),
        "raw cookie is absent from journal truth"
    );
    assert!(
        !encoded.contains(&csrf),
        "raw CSRF is absent from journal truth"
    );
    assert!(
        !encoded.contains(request["attempt_id"].as_str().expect("attempt wire")),
        "raw browser attempt handle is absent from journal truth"
    );
    let hires: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::AgentHired {
                id,
                trusted,
                browser_onboarding: Some(meta),
                ..
            } => {
                assert!(!trusted);
                assert_eq!(meta.intended_goal.id.to_string(), goal);
                Some(id)
            }
            _ => None,
        })
        .collect();
    let goals: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::GoalSet {
                agent: owner,
                goal: set,
                browser_onboarding: Some(meta),
                browser_goal: None,
                goal_replacement: None,
            } => {
                assert_eq!(meta.attempt_id.bytes(), [0; 32]);
                assert_eq!(owner.to_string(), agent);
                assert_eq!(set.id.to_string(), goal);
                Some(set)
            }
            _ => None,
        })
        .collect();
    assert_eq!(hires.len(), 1);
    assert_eq!(goals.len(), 1);
    let check = goals[0]
        .completion_check
        .as_ref()
        .expect("the hire records the enrolled check's name");
    let revision = common::check_revision_as("default", "Checks pass", &[true_program()], 10, true);
    assert_eq!(
        (
            check.id.as_str(),
            check.label.as_str(),
            serde_json::to_value(check.revision).unwrap()
        ),
        ("default", "Checks pass", json!(revision))
    );
    let (frame, _) = snapshot(addr, &cookie, &csrf);
    let row = frame["projection"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == agent)
        .unwrap();
    assert_eq!(
        row["goal"]["completion_check"],
        json!({"id": "default", "revision": revision, "label": "Checks pass"}),
        "the Floor reads the name from the snapshot"
    );
    fixture.await_record(
        "TurnSpawned by the daemon-owned stub",
        |record| match record {
            Record::TurnSpawned { .. } => Some(()),
            _ => None,
        },
    );
    fixture.await_record(
        "TurnEnded by the daemon-owned stub",
        |record| match record {
            Record::TurnEnded { .. } => Some(()),
            _ => None,
        },
    );
    let effect_counts = |records: &[Record]| -> (usize, usize, usize, usize) {
        (
            records
                .iter()
                .filter(|record| {
                    matches!(
                        record,
                        Record::AgentHired {
                            browser_onboarding: Some(_),
                            ..
                        }
                    )
                })
                .count(),
            records
                .iter()
                .filter(|record| {
                    matches!(
                        record,
                        Record::GoalSet {
                            browser_onboarding: Some(_),
                            ..
                        }
                    )
                })
                .count(),
            records
                .iter()
                .filter(|record| matches!(record, Record::TurnIssued { .. }))
                .count(),
            records
                .iter()
                .filter(|record| matches!(record, Record::TurnSpawned { .. }))
                .count(),
        )
    };
    let before = effect_counts(&fixture.records());
    let (_, retry) = post_onboard(addr, &cookie, &csrf, &request);
    assert_eq!(retry["agent"], agent);
    assert_eq!(retry["goal"], goal);
    assert_eq!(
        effect_counts(&fixture.records()),
        before,
        "retry is read-only"
    );
    assert_eq!(before, (1, 1, 1, 1));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one authenticated session exercises each admission and cumulative hire gate"
)]
fn browser_admission_and_four_hire_limit_are_journal_derived() {
    let fixture = Fixture::new();
    let repo = fixture.state.path().join("enrolled-repo");
    std::fs::create_dir_all(repo.join(".git")).expect("repository marker");
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.enrollment = Enrollment {
        profiles: vec![Profile {
            id: "repo_1".into(),
            label: "Service workspace".into(),
            checks: vec![passing_check()],
            cwd: repo.canonicalize().expect("canonical repository"),
            written_cwd: repo.canonicalize().expect("canonical repository"),
            department: "engineering".into(),
            isolation: IsolationMode::Default,
            agent_budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            goal_budget: GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
            hop_cap: 3,
            mark: None,
        }],
    };
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
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let mut request = json!({
        "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
    });
    let initial = fixture.records().len();
    for (bad_cookie, bad_csrf) in [("invalid", csrf.as_str()), (cookie.as_str(), "invalid")] {
        let (head, body) = post_onboard(addr, bad_cookie, bad_csrf, &request);
        assert!(head.starts_with("HTTP/1.1 403 Forbidden"), "{head}");
        assert!(body.is_null());
    }
    for (key, value) in [("cwd", json!("/tmp/escape")), ("provider", json!("other"))] {
        let mut bad = request.clone();
        bad[key] = value;
        let (head, body) = post_onboard(addr, &cookie, &csrf, &bad);
        assert!(head.starts_with("HTTP/1.1 400 Bad Request"), "{head}");
        assert!(body.is_null());
    }
    request["attempt_id"] = json!("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB");
    let (head, _) = post_onboard(addr, &cookie, &csrf, &request);
    assert!(head.starts_with("HTTP/1.1 400 Bad Request"), "{head}");
    assert_eq!(fixture.records().len(), initial);

    let mut first = None;
    for final_char in ["A", "E", "I", "M"] {
        request["attempt_id"] = json!(format!("{}{}", "A".repeat(42), final_char));
        let (head, outcome) = post_onboard(addr, &cookie, &csrf, &request);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        if first.is_none() {
            first = Some((request.clone(), outcome.clone()));
        }
        if final_char == "I" {
            let (frame, _) = snapshot(addr, &cookie, &csrf);
            assert!(
                frame["onboarding"].is_object(),
                "three hires leave one slot"
            );
        }
    }
    let first_agent =
        serde_json::from_value(first.as_ref().expect("first hire").1["agent"].clone())
            .expect("first AgentId");
    running
        .handle
        .ask(Ask::Retire(Box::new(RetireParams {
            agent: first_agent,
            reason: "operator retired".into(),
        })))
        .expect("retire admitted");
    request["attempt_id"] = json!(format!("{}Q", "A".repeat(42)));
    let (head, body) = post_onboard(addr, &cookie, &csrf, &request);
    assert!(head.starts_with("HTTP/1.1 403 Forbidden"), "{head}");
    assert!(body.is_null());
    let hires = fixture
        .records()
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::AgentHired {
                    browser_onboarding: Some(_),
                    ..
                }
            )
        })
        .count();
    assert_eq!(hires, 4);
    let (first_request, first_outcome) = first.expect("first hire");
    let (_, retry) = post_onboard(addr, &cookie, &csrf, &first_request);
    assert_eq!(retry, first_outcome);
    let (frame, _) = snapshot(addr, &cookie, &csrf);
    assert!(
        frame.get("onboarding").is_none(),
        "four durable hires hide this session's catalog"
    );
    let (second_cookie, second_csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (other, _) = snapshot(addr, &second_cookie, &second_csrf);
    assert!(
        other["onboarding"].is_object(),
        "a separate session has independent slots"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_post_rejects_ambiguous_authority_framing_and_json_before_effect() {
    browser_post_rejection(None);
}

macro_rules! rejection_case {
    ($name:ident, $label:literal) => {
        #[test]
        fn $name() {
            browser_post_rejection(Some($label));
        }
    };
}

rejection_case!(browser_post_rejects_wrong_host, "wrong Host");
rejection_case!(browser_post_rejects_null_origin, "null Origin");
rejection_case!(browser_post_rejects_duplicate_host, "duplicate Host");
rejection_case!(browser_post_rejects_duplicate_csrf, "duplicate CSRF");
rejection_case!(
    browser_post_rejects_duplicate_json_member,
    "duplicate JSON member"
);
rejection_case!(browser_post_rejects_bad_workspace_id, "bad workspace id");
rejection_case!(
    browser_post_rejects_noncanonical_attempt,
    "noncanonical attempt"
);
rejection_case!(browser_post_rejects_array, "an array");
rejection_case!(browser_post_rejects_overlarge_body, "overlarge body");

#[expect(
    clippy::too_many_lines,
    reason = "one parser table shares the same no-effect journal oracle"
)]
fn browser_post_rejection(selected: Option<&str>) {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
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
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let host = addr.to_string();
    let body = r#"{"attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","workspace_id":"repo_1","name":"Kim","role":"engineer","goal_text":"ship"}"#;
    // An identity no other row uses, so the array row's attempt is free unless it is admitted.
    let fresh = format!("B{}", "A".repeat(42));
    let normal_headers = format!(
        "Host: {host}\r\nOrigin: http://{host}\r\nCookie: dudes_floor_session={cookie}\r\nX-Dudes-CSRF: {csrf}\r\nContent-Type: application/json\r\n"
    );
    let make = |headers: &str, body: &str| {
        format!(
            "POST /floor/onboard HTTP/1.1\r\n{headers}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    let cases = [
        (
            "wrong Host",
            normal_headers.replace(&format!("Host: {host}"), "Host: invalid"),
            body.to_owned(),
            403,
        ),
        (
            "null Origin",
            normal_headers.replace(&format!("Origin: http://{host}"), "Origin: null"),
            body.to_owned(),
            403,
        ),
        (
            "duplicate Host",
            format!("{normal_headers}Host: {host}\r\n"),
            body.to_owned(),
            400,
        ),
        (
            "duplicate CSRF",
            format!("{normal_headers}X-Dudes-CSRF: {csrf}\r\n"),
            body.to_owned(),
            400,
        ),
        (
            "duplicate JSON member",
            normal_headers.clone(),
            body.replace("\"name\":\"Kim\"", "\"name\":\"Kim\",\"name\":\"Other\""),
            400,
        ),
        (
            "bad workspace id",
            normal_headers.clone(),
            body.replace("repo_1", "../repo"),
            400,
        ),
        (
            "noncanonical attempt",
            normal_headers.clone(),
            body.replace(
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB",
            ),
            400,
        ),
        (
            "an array",
            normal_headers.clone(),
            format!(r#"["{fresh}","repo_1","Kim","engineer","ship"]"#),
            400,
        ),
        (
            "overlarge body",
            normal_headers.clone(),
            "x".repeat(4097),
            413,
        ),
    ];
    let before = fixture.records();
    for (label, headers, body, status) in cases {
        if selected.is_some_and(|selected| selected != label) {
            continue;
        }
        let (head, returned) = raw_onboard(addr, make(&headers, &body).as_bytes());
        assert!(
            head.starts_with(&format!("HTTP/1.1 {status} ")),
            "{label}: {head}"
        );
        assert!(
            returned.is_empty(),
            "{label}: refusal has no domain envelope"
        );
        assert!(
            head.contains("Cache-Control: no-store"),
            "{label}: no cache"
        );
        assert!(
            !head.to_ascii_lowercase().contains("access-control-allow"),
            "{label}: no CORS"
        );
    }
    assert_eq!(fixture.records(), before, "invalid requests append nothing");
    // Nor did the array reserve its attempt: a different request under that identity completes,
    // where a held attempt would answer 202 and a recorded one 409.
    let (head, response) = post_onboard(
        addr,
        &cookie,
        &csrf,
        &json!({
            "attempt_id": fresh, "workspace_id": "repo_1", "name": "Other", "role": "engineer",
            "goal_text": "ship"
        }),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(response["state"], "complete");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn refused_body_drain_discards_a_pipelined_action_without_effects() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
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
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let probe = daemon.floor_gateway_probe().expect("Floor probe");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let headers = format!(
        "Host: {addr}\r\nOrigin: http://{addr}\r\nCookie: dudes_floor_session={cookie}\r\nX-Dudes-CSRF: {csrf}\r\nContent-Type: application/json\r\n"
    );
    let before = fixture.records();
    let mut client = TcpStream::connect(addr).expect("refused client");
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("response deadline");
    client
        .write_all(
            format!("POST /floor/onboard HTTP/1.1\r\n{headers}Content-Length: 4097\r\n\r\n")
                .as_bytes(),
        )
        .expect("overlarge head");
    let head = read_head(&mut client);
    assert!(head.starts_with("HTTP/1.1 413 "), "{head}");
    assert!(
        head.contains("Content-Length: 0\r\n"),
        "no refusal envelope"
    );
    assert!(head.contains("Cache-Control: no-store"));
    assert!(!head.to_ascii_lowercase().contains("access-control-allow"));
    let mut byte = [0];
    assert_eq!(client.read(&mut byte).expect("refusal EOF"), 0);
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(
        probe.live_connections(),
        1,
        "the refused body is still draining"
    );
    let body = json!({"attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"});
    let encoded = serde_json::to_vec(&body).expect("valid action body");
    let tail = format!(
        "POST /floor/onboard HTTP/1.1\r\n{headers}Content-Length: {}\r\n\r\n",
        encoded.len()
    );
    client.write_all(tail.as_bytes()).expect("pipelined head");
    client.write_all(&encoded).expect("pipelined body");
    client
        .shutdown(std::net::Shutdown::Write)
        .expect("finish discarded bytes");
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while probe.live_connections() != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "discarded body did not close"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        fixture.records(),
        before,
        "the valid tail caused no journal effect"
    );
    let (head, response) = post_onboard(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(
        response["state"], "complete",
        "discard did not reserve the attempt"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The request digest over the fields as the parser trims and admits them.
fn onboard_digest(workspace_id: &str, name: &str, role: &str, goal_text: &str) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"dudes.onboard.v1\0");
    for (field, value) in [
        ("workspace_id", workspace_id),
        ("name", name),
        ("role", role),
        ("goal_text", goal_text),
    ] {
        hasher.update(u16::try_from(field.len()).unwrap().to_be_bytes());
        hasher.update(field.as_bytes());
        hasher.update(u32::try_from(value.len()).unwrap().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.finalize().into()
}

/// A new onboarding attempt whose `goal_text` holds U+0000 is
/// refused in the new-attempt validation with the route's existing empty `400`, before
/// any effect: no `AgentHired` and no `GoalSet`. That text would become the agent's goal, and no
/// launch can carry U+0000.
///
/// Control, first: the same request with U+0001 in its place completes.
///
/// Durable retry: an attempt made durable before the refusal existed, planted with [`seed`] as a
/// complete hire and goal whose recorded goal holds U+0000, still answers a same-key, same-payload
/// retry from the journal-derived index with its original `200`.
///
/// Mutants: deleting the check hires the U+0000 attempt; refusing every `Cc` refuses the control;
/// moving the check into `OnboardRequest::parse` refuses the durable retry.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one planted journal serves the control, the refusal and the durable retry"
)]
fn browser_onboarding_goal_text_holding_nul_is_refused_before_effect() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
    let profile = settings.enrollment.profiles[0].clone();
    let planted_agent = AgentId::from_parts(7, 1);
    let planted_goal = dudes_proto::Goal {
        completion_check: None,
        id: dudes_types::GoalId::from_parts(7, 2),
        text: "ship\u{0}it".to_owned(),
        predicate: profile.checks[0].predicate.clone(),
        budget: profile.goal_budget,
        hop_cap: profile.hop_cap,
        baseline: None,
    };
    // Forty-three `Q`s decode to the bytes 0x41 0x04 0x10, repeated; the index keys on those.
    let durable = "Q".repeat(43);
    let attempt = Hex32::new(std::array::from_fn(|k| [0x41, 0x04, 0x10][k % 3]));
    let admitting = Hex32::new([7; 32]);
    seed(
        &fixture,
        [
            Record::AgentHired {
                character: None,
                accent: None,
                id: planted_agent,
                name: "Kim".to_owned(),
                role: "engineer".to_owned(),
                department: profile.department.clone(),
                provider: Provider::Claude,
                cwd: profile.cwd.clone(),
                budget: profile.agent_budget,
                sandbox: None,
                isolation: Some(profile.isolation),
                max_turns: None,
                max_budget: None,
                trusted: false,
                workspace: None,
                worktree: None,
                mcp_servers: Vec::new(),
                browser_onboarding: Some(Box::new(BrowserOnboardingHire {
                    attempt_id: attempt,
                    request_digest: Hex32::new(onboard_digest(
                        "repo_1",
                        "Kim",
                        "engineer",
                        "ship\u{0}it",
                    )),
                    profile_id: profile.id.clone(),
                    session_digest: admitting,
                    intended_goal: planted_goal.clone(),
                })),
            },
            Record::GoalSet {
                agent: planted_agent,
                goal: planted_goal.clone(),
                browser_onboarding: Some(BrowserOnboardingGoal {
                    attempt_id: attempt,
                    session_digest: admitting,
                }),
                browser_goal: None,
                goal_replacement: None,
            },
        ],
    );
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
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let request = |attempt: &str, goal_text: &str| {
        json!({
            "attempt_id": attempt, "workspace_id": "repo_1", "name": "Kim", "role": "engineer",
            "goal_text": goal_text
        })
    };
    let effects = |records: &[Record]| {
        records
            .iter()
            .filter(|record| matches!(record, Record::AgentHired { .. } | Record::GoalSet { .. }))
            .count()
    };

    let (head, complete) = post_onboard(
        addr,
        &cookie,
        &csrf,
        &request(&"A".repeat(43), "ship\u{1}it"),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "control: {head}");
    assert_eq!(complete["state"], "complete", "{complete}");
    assert!(
        fixture.records().iter().any(|record| matches!(
            record,
            Record::GoalSet { goal, browser_onboarding: Some(_), .. } if goal.text == "ship\u{1}it"
        )),
        "control: the initial goal is recorded with the bytes sent"
    );

    let before = effects(&fixture.records());
    let refused_attempt = format!("AQ{}", "A".repeat(41));
    let (head, refused) = post_onboard(
        addr,
        &cookie,
        &csrf,
        &request(&refused_attempt, "ship\u{0}it"),
    );
    assert!(head.starts_with("HTTP/1.1 400 Bad Request"), "{head}");
    assert!(refused.is_null(), "the refusal has no onboarding envelope");
    assert_eq!(
        effects(&fixture.records()),
        before,
        "no AgentHired and no GoalSet"
    );

    let mut older = request(&durable, "ship\u{0}it");
    older["check_id"] = Value::Null;
    let (head, retried) = post_onboard(addr, &cookie, &csrf, &older);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "durable retry: {head}");
    assert_eq!(
        retried,
        json!({
            "attempt_id": durable, "state": "complete",
            "agent": planted_agent, "goal": planted_goal.id
        }),
        "the index answers with the original ids"
    );
    assert_eq!(
        effects(&fixture.records()),
        before,
        "the retry appends nothing"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn concurrent_same_key_is_pending_and_changed_payload_refuses() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
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
    .expect("boot Floor");
    let pause = daemon.onboarding_pause();
    pause.arm(OnboardingPausePoint::BeforeHireAppend);
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let request = json!({
        "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
    });
    let owner_cookie = cookie.clone();
    let owner_csrf = csrf.clone();
    let owner_request = request.clone();
    let owner =
        std::thread::spawn(move || post_onboard(addr, &owner_cookie, &owner_csrf, &owner_request));
    assert!(
        pause.wait_reached(Duration::from_secs(3)),
        "writer reached the first append cut"
    );
    let (head, pending) = post_onboard(addr, &cookie, &csrf, &request);
    assert!(head.starts_with("HTTP/1.1 202 Accepted"), "{head}");
    assert_eq!(
        pending,
        json!({"attempt_id":request["attempt_id"], "state":"pending"})
    );
    assert!(pending.get("agent").is_none() && pending.get("goal").is_none());
    let mut changed = request.clone();
    changed["name"] = json!("Other");
    let (head, conflict) = post_onboard(addr, &cookie, &csrf, &changed);
    assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
    assert!(conflict.is_null());
    assert!(!fixture.records().iter().any(|record| matches!(
        record,
        Record::AgentHired {
            browser_onboarding: Some(_),
            ..
        }
    )));
    pause.release();
    let (head, complete) = owner.join().expect("owner response");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(complete["state"], "complete");
    let (_, retry) = post_onboard(addr, &cookie, &csrf, &request);
    assert_eq!(retry, complete);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                Record::AgentHired {
                    browser_onboarding: Some(_),
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn local_goal_and_retirement_win_before_browser_initial_goal() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
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
    .expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);

    for (last, retire) in [("A", false), ("E", true)] {
        pause.arm(OnboardingPausePoint::BetweenEffects);
        let request = json!({
            "attempt_id":format!("{}{}", "A".repeat(42), last),
            "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
        });
        let owner_cookie = cookie.clone();
        let owner_csrf = csrf.clone();
        let owner_request = request.clone();
        let owner = std::thread::spawn(move || {
            post_onboard(addr, &owner_cookie, &owner_csrf, &owner_request)
        });
        assert!(
            pause.wait_reached(Duration::from_secs(3)),
            "hire folded before goal continuation"
        );
        let agent = fixture
            .records()
            .iter()
            .rev()
            .find_map(|record| match record {
                Record::AgentHired {
                    id,
                    browser_onboarding: Some(_),
                    ..
                } => Some(*id),
                _ => None,
            })
            .expect("durable hire");
        let local = if retire {
            running
                .handle
                .ask_queued(Ask::Retire(Box::new(RetireParams {
                    agent,
                    reason: "operator retired".into(),
                })))
                .expect("retire queued")
        } else {
            running
                .handle
                .ask_queued(Ask::SetGoal(Box::new(common::goal_params(
                    agent,
                    &[true_program()],
                ))))
                .expect("local goal queued")
        };
        pause.release();
        let answer = local
            .recv_timeout(Duration::from_secs(3))
            .expect("local writer answer")
            .expect("local action admitted");
        let (head, partial) = owner.join().expect("browser response");
        assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
        assert_eq!(partial["state"], "hired_without_goal");
        assert_eq!(partial["agent"], agent.to_string());
        assert!(partial["goal"].is_null());
        if retire {
            assert_eq!(partial["reason"], "agent_retired");
            assert_eq!(partial["retired"], true);
            assert!(partial["current_goal"].is_null());
        } else {
            let Answered::GoalSet(set) = answer else {
                panic!("local goal answer")
            };
            assert_eq!(partial["reason"], "goal_conflict");
            assert_eq!(partial["current_goal"], set.goal.to_string());
            assert_eq!(partial["retired"], false);
        }
        assert!(!fixture.records().iter().any(|record| matches!(record,
            Record::GoalSet { agent: owner, browser_onboarding: Some(_), .. } if *owner == agent
        )));
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "both durable local winners require the same live and replay controls"
)]
fn uncertain_local_goal_and_retirement_fence_browser_continuation() {
    for (cut, retire) in [
        (OnboardingFailurePoint::LocalGoal, false),
        (OnboardingFailurePoint::LocalRetire, true),
    ] {
        let fixture = Fixture::new();
        let settings = enrolled_settings(&fixture);
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
        .expect("boot Floor");
        let pause = daemon.onboarding_pause();
        let fault = daemon.onboarding_failure();
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let request = json!({
            "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
        });
        pause.arm(OnboardingPausePoint::BetweenEffects);
        let owner_cookie = cookie.clone();
        let owner_csrf = csrf.clone();
        let owner_request = request.clone();
        let owner = std::thread::spawn(move || {
            post_onboard(addr, &owner_cookie, &owner_csrf, &owner_request)
        });
        assert!(pause.wait_reached(Duration::from_secs(3)), "durable hire");
        let agent = fixture
            .records()
            .iter()
            .find_map(|record| match record {
                Record::AgentHired {
                    id,
                    browser_onboarding: Some(_),
                    ..
                } => Some(*id),
                _ => None,
            })
            .expect("attributed hire");
        fault.arm(cut);
        let local = if retire {
            running
                .handle
                .ask_queued(Ask::Retire(Box::new(RetireParams {
                    agent,
                    reason: "operator retired".into(),
                })))
                .expect("retire queued")
        } else {
            running
                .handle
                .ask_queued(Ask::SetGoal(Box::new(common::goal_params(
                    agent,
                    &[true_program()],
                ))))
                .expect("local goal queued")
        };
        pause.release();
        assert!(
            local
                .recv_timeout(Duration::from_secs(3))
                .expect("local writer answer")
                .is_err(),
            "{cut:?}: injected failure fired after fsync"
        );
        let (head, body) = owner.join().expect("browser response");
        assert!(
            head.starts_with("HTTP/1.1 503 Service Unavailable"),
            "{cut:?}: {head}"
        );
        assert!(
            body.is_null(),
            "{cut:?}: uncertain truth has no response IDs"
        );
        let records = fixture.records();
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record,
                    Record::GoalSet { agent: owner, browser_onboarding: Some(_), .. }
                        if *owner == agent
                ))
                .count(),
            0,
            "{cut:?}: browser must not overwrite durable local winner"
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| if retire {
                    matches!(record, Record::AgentRetired { id, .. } if *id == agent)
                } else {
                    matches!(record, Record::GoalSet { agent: owner, browser_onboarding: None, .. }
                        if *owner == agent)
                })
                .count(),
            1,
            "{cut:?}: local winner is durable"
        );
        let (pending_head, pending) = post_onboard(addr, &cookie, &csrf, &request);
        assert!(
            pending_head.starts_with("HTTP/1.1 202 Accepted"),
            "{cut:?}: {pending_head}"
        );
        assert_eq!(pending["state"], "pending");
        assert!(
            snapshot_optional(addr, &cookie, &csrf).is_none(),
            "{cut:?}: unresolved fold closes normal snapshot publication"
        );
        let mut fresh_request = request.clone();
        fresh_request["attempt_id"] = json!(format!("{}E", "A".repeat(42)));
        let (fresh_head, fresh_body) = post_onboard(addr, &cookie, &csrf, &fresh_request);
        assert!(
            fresh_head.starts_with("HTTP/1.1 503 Service Unavailable"),
            "{cut:?}: {fresh_head}"
        );
        assert!(fresh_body.is_null());
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

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
        .expect("replay local winner");
        let addr = daemon.floor_addr().expect("replayed Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let (head, partial) = post_onboard(addr, &cookie, &csrf, &request);
        assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{cut:?}: {head}");
        assert_eq!(partial["state"], "hired_without_goal");
        assert_eq!(partial["agent"], agent.to_string());
        assert_eq!(partial["retired"], retire);
        assert_eq!(
            partial["reason"],
            if retire {
                "agent_retired"
            } else {
                "goal_conflict"
            }
        );
        assert_eq!(partial["current_goal"].is_null(), retire);
        assert!(!fixture.records().iter().any(|record| matches!(record,
            Record::GoalSet { agent: owner, browser_onboarding: Some(_), .. }
                if *owner == agent
        )));
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one fenced run asserts complete, conflicting, partial and fresh attempts"
)]
fn fenced_complete_retry_returns_original_ids_without_effects() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
    let host = SharedHost::new();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let fault = daemon.onboarding_failure();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let complete_request = json!({
        "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
    });
    let (head, complete) = post_onboard(addr, &cookie, &csrf, &complete_request);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(complete["state"], "complete");
    let mut partial_request = complete_request.clone();
    partial_request["attempt_id"] = json!(format!("{}E", "A".repeat(42)));
    pause.arm(OnboardingPausePoint::BetweenEffects);
    let owner_cookie = cookie.clone();
    let owner_csrf = csrf.clone();
    let owner_request = partial_request.clone();
    let owner =
        std::thread::spawn(move || post_onboard(addr, &owner_cookie, &owner_csrf, &owner_request));
    assert!(pause.wait_reached(Duration::from_secs(3)), "durable hire");
    let partial_agent = fixture
        .records()
        .iter()
        .find_map(|record| match record {
            Record::AgentHired {
                character: None,
                accent: None,
                id,
                browser_onboarding: Some(_),
                ..
            } if id.to_string() != complete["agent"] => Some(*id),
            _ => None,
        })
        .expect("second attributed hire");
    fault.arm(OnboardingFailurePoint::LocalGoal);
    let local = running
        .handle
        .ask_queued(Ask::SetGoal(Box::new(common::goal_params(
            partial_agent,
            &[true_program()],
        ))))
        .expect("local goal queued");
    pause.release();
    assert!(
        local
            .recv_timeout(Duration::from_secs(3))
            .expect("local writer answer")
            .is_err(),
        "control: injected failure fired after fsync"
    );
    let (head, body) = owner.join().expect("partial owner response");
    assert!(
        head.starts_with("HTTP/1.1 503 Service Unavailable"),
        "control: fence is up: {head}"
    );
    assert!(body.is_null());
    let before = fixture.records();
    let spawned = host.spawned().len();

    for retry in 1..=2 {
        let (head, again) = post_onboard(addr, &cookie, &csrf, &complete_request);
        assert!(
            head.starts_with("HTTP/1.1 200 OK"),
            "complete retry {retry} while fenced: {head}"
        );
        assert_eq!(again, complete, "complete retry {retry} keeps original IDs");
        let mut changed = complete_request.clone();
        changed["name"] = json!("Other");
        let (head, conflict) = post_onboard(addr, &cookie, &csrf, &changed);
        assert!(
            head.starts_with("HTTP/1.1 409 Conflict"),
            "changed payload {retry} while fenced: {head}"
        );
        assert!(conflict.is_null());
    }
    let (head, pending) = post_onboard(addr, &cookie, &csrf, &partial_request);
    assert!(
        head.starts_with("HTTP/1.1 202 Accepted"),
        "known partial stays pending while fenced: {head}"
    );
    assert_eq!(pending["state"], "pending");
    assert!(pending.get("agent").is_none() && pending.get("goal").is_none());
    let mut fresh_request = complete_request.clone();
    fresh_request["attempt_id"] = json!(format!("{}I", "A".repeat(42)));
    let (head, fresh) = post_onboard(addr, &cookie, &csrf, &fresh_request);
    assert!(
        head.starts_with("HTTP/1.1 503 Service Unavailable"),
        "fresh attempt stays fenced: {head}"
    );
    assert!(fresh.is_null());
    assert_eq!(
        fixture.records(),
        before,
        "fenced retries append no hire, goal, decision or turn"
    );
    assert_eq!(
        host.spawned().len(),
        spawned,
        "fenced retries spawn nothing"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn session_expiry_before_each_append_has_distinct_truthful_outcomes() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let request = json!({
        "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
    });
    pause.arm(OnboardingPausePoint::BeforeHireAppend);
    let owner_request = request.clone();
    let owner = std::thread::spawn(move || post_onboard(addr, &cookie, &csrf, &owner_request));
    assert!(
        pause.wait_reached(Duration::from_secs(3)),
        "first permit cut"
    );
    clock.advance(8 * 60 * 60 * 1000);
    pause.release();
    let (head, body) = owner.join().expect("no-effect response");
    assert!(head.starts_with("HTTP/1.1 403 Forbidden"), "{head}");
    assert!(body.is_null());
    assert!(!fixture.records().iter().any(|record| matches!(
        record,
        Record::AgentHired {
            browser_onboarding: Some(_),
            ..
        }
    )));

    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    pause.arm(OnboardingPausePoint::BeforeGoalPermit);
    let owner_request = request.clone();
    let owner_cookie = cookie.clone();
    let owner_csrf = csrf.clone();
    let owner =
        std::thread::spawn(move || post_onboard(addr, &owner_cookie, &owner_csrf, &owner_request));
    assert!(
        pause.wait_reached(Duration::from_secs(3)),
        "second permit cut"
    );
    let durable_hire = fixture
        .records()
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::AgentHired {
                    browser_onboarding: Some(_),
                    ..
                }
            )
        })
        .count();
    assert_eq!(durable_hire, 1);
    clock.advance(8 * 60 * 60 * 1000);
    pause.release();
    let (head, body) = owner.join().expect("partial response");
    assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
    assert_eq!(body["state"], "hired_without_goal");
    assert_eq!(body["reason"], "session_expired");
    assert!(body["agent"].is_string());
    assert!(body["goal"].is_null());
    assert!(!fixture.records().iter().any(|record| matches!(
        record,
        Record::GoalSet {
            browser_onboarding: Some(_),
            ..
        }
    )));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn expired_follower_after_registry_lock_wait_gets_empty_forbidden() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .expect("boot Floor");
    let pause = daemon.onboarding_pause();
    pause.arm(OnboardingPausePoint::AfterGoalPermit);
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let request = json!({
        "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
    });
    let owner_cookie = cookie.clone();
    let owner_csrf = csrf.clone();
    let owner_request = request.clone();
    let owner =
        std::thread::spawn(move || post_onboard(addr, &owner_cookie, &owner_csrf, &owner_request));
    assert!(
        pause.wait_reached(Duration::from_secs(3)),
        "owner holds the goal append permit"
    );
    let (sent_tx, sent_rx) = std::sync::mpsc::channel();
    let follower_cookie = cookie.clone();
    let follower_csrf = csrf.clone();
    let follower_request = request.clone();
    let follower = std::thread::spawn(move || {
        post_onboard_signaled(
            addr,
            &follower_cookie,
            &follower_csrf,
            &follower_request,
            Some(sent_tx),
        )
    });
    sent_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("same-key follower sent while registry locked");
    clock.advance(8 * 60 * 60 * 1000);
    pause.release();
    let (owner_head, owner_body) = owner.join().expect("owner response");
    assert!(
        owner_head.starts_with("HTTP/1.1 200 OK"),
        "held permit wins through fsync: {owner_head}"
    );
    assert_eq!(owner_body["state"], "complete");
    let (head, body) = follower.join().expect("follower response");
    assert!(
        head.starts_with("HTTP/1.1 403 Forbidden"),
        "expired follower must not get 202: {head}"
    );
    assert!(body.is_null(), "no typed attempt ID after expiry");
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                Record::AgentHired {
                    browser_onboarding: Some(_),
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                Record::GoalSet {
                    browser_onboarding: Some(_),
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn expired_same_key_follower_blocked_in_reservation_gets_empty_forbidden() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let authorizer = daemon.floor_session_authorizer().expect("session registry");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let host = addr.to_string();
    let origin = format!("http://{host}");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    // Idle-fresh up to one minute short of the absolute bound, so only that bound can pass.
    for _ in 0..16 {
        clock.advance(29 * 60 * 1000);
        assert!(
            authorizer
                .authorize_read(&host, &origin, &cookie, &csrf)
                .is_some(),
            "activity keeps the session idle-fresh"
        );
    }
    clock.advance(15 * 60 * 1000);
    let request = json!({
        "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
    });
    pause.arm(OnboardingPausePoint::BeforeHireAppend);
    let owner_cookie = cookie.clone();
    let owner_csrf = csrf.clone();
    let owner_request = request.clone();
    let owner =
        std::thread::spawn(move || post_onboard(addr, &owner_cookie, &owner_csrf, &owner_request));
    assert!(
        pause.wait_reached(Duration::from_secs(3)),
        "owner holds the attempt reservation before its hire permit"
    );
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    authorizer.before_next_reserve_for_test(move || {
        reached_tx.send(()).expect("reservation reached");
        let _ = go_rx.recv_timeout(Duration::from_secs(5));
    });
    let follower = std::thread::spawn(move || post_onboard(addr, &cookie, &csrf, &request));
    let reached = reached_rx.recv_timeout(Duration::from_secs(3)).is_ok();
    clock.advance(60 * 1000);
    let _ = go_tx.send(());
    let (head, body) = follower.join().expect("follower response");
    // Release the paused owner before asserting, so a failure reports instead of hanging shutdown.
    pause.release();
    let (owner_head, owner_body) = owner.join().expect("owner response");
    assert!(
        reached,
        "same-key follower admitted and waiting inside reservation"
    );
    assert!(
        head.starts_with("HTTP/1.1 403 Forbidden"),
        "a follower that expired inside reservation must not get 202: {head}"
    );
    assert!(body.is_null(), "no typed attempt ID after expiry: {body}");
    assert!(
        owner_head.starts_with("HTTP/1.1 403 Forbidden"),
        "{owner_head}"
    );
    assert!(owner_body.is_null());
    assert!(
        !fixture.records().iter().any(|record| matches!(
            record,
            Record::AgentHired {
                browser_onboarding: Some(_),
                ..
            } | Record::GoalSet {
                browser_onboarding: Some(_),
                ..
            }
        )),
        "no hire and no goal after expiry"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the three boot phases assert one partial attempt through replay and retry"
)]
fn replayed_partial_finishes_only_its_preminted_goal_and_complete_retry_is_read_only() {
    for appearance in [
        json!({}),
        json!({"character":"h"}),
        json!({"accent":"sky"}),
        json!({"character":"h","accent":"sky"}),
    ] {
        let fixture = Fixture::new();
        let settings = enrolled_settings(&fixture);
        let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
        let daemon = boot(
            &settings,
            seams(
                &settings,
                SharedHost::new(),
                Arc::new(ScriptedRunner::default()),
                Arc::clone(&clock),
                Witness::default(),
            ),
        )
        .expect("first boot");
        let pause = daemon.onboarding_pause();
        pause.arm(OnboardingPausePoint::BeforeGoalPermit);
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let mut request = json!({
            "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
        });
        request
            .as_object_mut()
            .unwrap()
            .extend(appearance.as_object().unwrap().clone());
        let owner_request = request.clone();
        let owner = std::thread::spawn(move || post_onboard(addr, &cookie, &csrf, &owner_request));
        assert!(
            pause.wait_reached(Duration::from_secs(3)),
            "durable hire precedes second permit"
        );
        let (original_agent, original_goal) = fixture
            .records()
            .iter()
            .find_map(|record| match record {
                Record::AgentHired {
                    id,
                    browser_onboarding: Some(meta),
                    ..
                } => Some((*id, meta.intended_goal.id)),
                _ => None,
            })
            .expect("recorded intended goal");
        clock.advance(8 * 60 * 60 * 1000);
        pause.release();
        let (_, partial) = owner.join().expect("partial response");
        assert_eq!(partial["reason"], "session_expired");
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

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
        .expect("replay partial hire");
        let addr = daemon.floor_addr().expect("new Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let before_conflict = fixture.records();
        for (field, value) in [("character", "c"), ("accent", "rose")] {
            let mut changed = request.clone();
            changed[field] = json!(value);
            let (head, conflict) = post_onboard(addr, &cookie, &csrf, &changed);
            assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{field}: {head}");
            assert!(conflict.is_null());
            assert_eq!(
                fixture.records(),
                before_conflict,
                "changed choice appends nothing"
            );
        }
        let (head, complete) = post_onboard(addr, &cookie, &csrf, &request);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert_eq!(complete["agent"], original_agent.to_string());
        assert_eq!(complete["goal"], original_goal.to_string());
        issued_goal_turn(&fixture, original_goal);
        running.handle.doctor().unwrap();
        let (frame, _) = snapshot(addr, &cookie, &csrf);
        let row = frame["projection"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == original_agent.to_string())
            .unwrap();
        assert_eq!(row.get("character"), appearance.get("character"));
        assert_eq!(row.get("accent"), appearance.get("accent"));
        let records = fixture.records();
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record,
                    Record::AgentHired {
                        browser_onboarding: Some(_),
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record,
                    Record::GoalSet {
                        browser_onboarding: Some(_),
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

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
        .expect("replay complete attempt");
        let addr = daemon.floor_addr().expect("third Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        issued_goal_turn(&fixture, original_goal);
        running.handle.doctor().unwrap();
        let before = fixture.records();
        let (_, replayed) = post_onboard(addr, &cookie, &csrf, &request);
        assert_eq!(replayed, complete);
        assert_eq!(
            fixture.records(),
            before,
            "complete retry emits no decision, turn or spawn"
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

#[test]
fn replayed_partial_refuses_every_effective_profile_change() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let fixture_parts = seams(
        &settings,
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::clone(&clock),
        Witness::default(),
    );
    let daemon = boot(&settings, fixture_parts).expect("first boot");
    let pause = daemon.onboarding_pause();
    pause.arm(OnboardingPausePoint::BeforeGoalPermit);
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let request = json!({
        "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
    });
    let owner_request = request.clone();
    let owner = std::thread::spawn(move || post_onboard(addr, &cookie, &csrf, &owner_request));
    assert!(
        pause.wait_reached(Duration::from_secs(3)),
        "durable partial hire"
    );
    clock.advance(8 * 60 * 60 * 1000);
    pause.release();
    let (_, partial) = owner.join().expect("partial result");
    assert_eq!(partial["reason"], "session_expired");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    let new_repo = fixture.state.path().join("different-repo");
    std::fs::create_dir_all(new_repo.join(".git")).expect("new repository marker");

    for change in [
        "removed",
        "cwd",
        "department",
        "isolation",
        "agent_budget",
        "goal_budget",
        "predicate_argv",
        "predicate_timeout",
        "hop_cap",
    ] {
        let mut changed = settings.clone();
        if change == "removed" {
            changed.enrollment.profiles.clear();
        } else {
            let profile = &mut changed.enrollment.profiles[0];
            match change {
                "cwd" => profile.cwd = new_repo.canonicalize().expect("canonical alternate"),
                "department" => profile.department = "infra".into(),
                "isolation" => profile.isolation = IsolationMode::Inherit,
                "agent_budget" => profile.agent_budget.usd_per_day = Some(Usd::from_dollars(11)),
                "goal_budget" => profile.goal_budget.usd = Some(Usd::from_dollars(6)),
                "predicate_argv" => {
                    profile.checks[0].predicate = Predicate::Command {
                        argv: vec!["/usr/bin/false".into()],
                        timeout_s: 10,
                    }
                }
                "predicate_timeout" => {
                    profile.checks[0].predicate = Predicate::Command {
                        argv: vec![true_program().into()],
                        timeout_s: 11,
                    }
                }
                "hop_cap" => profile.hop_cap = 4,
                _ => unreachable!(),
            }
        }
        let daemon = boot(
            &changed,
            seams(
                &changed,
                SharedHost::new(),
                Arc::new(ScriptedRunner::default()),
                Arc::new(TestClock::new()),
                Witness::default(),
            ),
        )
        .expect("replay with changed profile");
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&changed).bootstrap);
        let before = fixture.records();
        let (head, outcome) = post_onboard(addr, &cookie, &csrf, &request);
        assert!(
            head.starts_with("HTTP/1.1 409 Conflict"),
            "{change}: {head}"
        );
        assert_eq!(outcome["reason"], "profile_changed", "{change}");
        assert_eq!(
            fixture.records(),
            before,
            "{change}: retry cannot replace the goal"
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one session registry proves both losing and winning permit orders"
)]
fn session_revocation_linearizes_with_both_append_permits() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::ticking(1_800_000_000_000, 0)),
            Witness::default(),
        ),
    )
    .expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let authorizer = daemon.floor_session_authorizer().expect("session registry");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let request = json!({
        "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
    });

    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    pause.arm(OnboardingPausePoint::BeforeHireAppend);
    let owner_request = request.clone();
    let owner_cookie = cookie.clone();
    let owner =
        std::thread::spawn(move || post_onboard(addr, &owner_cookie, &csrf, &owner_request));
    assert!(
        pause.wait_reached(Duration::from_secs(3)),
        "first permit cut"
    );
    assert!(authorizer.revoke_for_test(&cookie));
    pause.release();
    let (head, body) = owner.join().expect("no-effect response");
    assert!(head.starts_with("HTTP/1.1 403 Forbidden"), "{head}");
    assert!(body.is_null());
    assert!(!fixture.records().iter().any(|record| matches!(
        record,
        Record::AgentHired {
            browser_onboarding: Some(_),
            ..
        }
    )));

    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    pause.arm(OnboardingPausePoint::BeforeGoalPermit);
    let owner_request = request.clone();
    let owner_cookie = cookie.clone();
    let owner =
        std::thread::spawn(move || post_onboard(addr, &owner_cookie, &csrf, &owner_request));
    assert!(
        pause.wait_reached(Duration::from_secs(3)),
        "second permit cut"
    );
    assert!(authorizer.revoke_for_test(&cookie));
    pause.release();
    let (head, body) = owner.join().expect("partial response");
    assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
    assert_eq!(body["reason"], "session_revoked");
    assert!(body["agent"].is_string() && body["goal"].is_null());

    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let mut next = request.clone();
    next["attempt_id"] = json!(format!("{}E", "A".repeat(42)));
    pause.arm(OnboardingPausePoint::AfterGoalPermit);
    let owner_request = next;
    let owner_cookie = cookie.clone();
    let owner =
        std::thread::spawn(move || post_onboard(addr, &owner_cookie, &csrf, &owner_request));
    assert!(
        pause.wait_reached(Duration::from_secs(3)),
        "goal append holds permit"
    );
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let revoke = authorizer.clone();
    let revoker = std::thread::spawn(move || {
        started_tx.send(()).expect("started signal");
        done_tx
            .send(revoke.revoke_for_test(&cookie))
            .expect("revoke result");
    });
    started_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("revoker started");
    assert!(
        done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "revocation waits while the append permit is held"
    );
    pause.release();
    assert!(
        done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("revocation completed")
    );
    revoker.join().expect("revoker thread");
    let (head, body) = owner.join().expect("complete response");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(body["state"], "complete");
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                Record::AgentHired {
                    browser_onboarding: Some(_),
                    ..
                }
            ))
            .count(),
        2
    );
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                Record::GoalSet {
                    browser_onboarding: Some(_),
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

fn issued_onboarding_fence(
    fixture: &Fixture,
    handle: &duded::Handle,
    agent: AgentId,
    goal: GoalId,
) {
    nth_turn(fixture, goal, 0);
    let before = fixture.records();
    let stopped = handle.ask(Ask::Pause(agent)).unwrap_err();
    assert_eq!(stopped.code, dudes_proto::rpc::ErrorCode::Refused.code());
    assert_eq!(
        stopped.message,
        "journal state unresolved; restart to replay before new work or writes"
    );
    assert_eq!(
        fixture.records(),
        before,
        "the fenced probe appends nothing"
    );
    let counts = (
        before.iter().filter(|record| matches!(record, Record::AgentHired { id, .. } if *id == agent)).count(),
        before.iter().filter(|record| matches!(record, Record::GoalSet { agent: owner, goal: issued, .. } if *owner == agent && issued.id == goal)).count(),
        before.iter().filter(|record| matches!(record, Record::TurnIssued { agent: owner, goal: issued, .. } if *owner == agent && *issued == goal)).count(),
        before.iter().filter(|record| matches!(record, Record::TurnSpawned { agent: owner, .. } if *owner == agent)).count(),
    );
    assert_eq!(counts, (1, 1, 1, 0));
}

fn completed_cut_reply(
    fixture: &Fixture,
    request: &Value,
    head: &str,
    body: &Value,
) -> (AgentId, GoalId) {
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    let (agent, goal) = fixture
        .records()
        .into_iter()
        .rev()
        .find_map(|record| match record {
            Record::AgentHired {
                id,
                browser_onboarding: Some(metadata),
                ..
            } => Some((id, metadata.intended_goal.id)),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        *body,
        json!({"attempt_id":request["attempt_id"], "state":"complete", "agent":agent, "goal":goal})
    );
    (agent, goal)
}

fn onboarding_hire_count(records: &[Record]) -> usize {
    records
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::AgentHired {
                    browser_onboarding: Some(_),
                    ..
                }
            )
        })
        .count()
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the journal cuts share one exact record and Spawn counter"
)]
fn post_fsync_cuts_replay_one_hire_goal_and_turn() {
    for cut in [
        OnboardingFailurePoint::Hire,
        OnboardingFailurePoint::HireProjection,
        OnboardingFailurePoint::Goal,
        OnboardingFailurePoint::TurnIssued,
    ] {
        let fixture = Fixture::new();
        let settings = enrolled_settings(&fixture);
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
        .expect("first boot");
        daemon.onboarding_failure().arm(cut);
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let request = json!({
            "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
        });
        let (head, body) = post_onboard(addr, &cookie, &csrf, &request);
        if cut != OnboardingFailurePoint::TurnIssued {
            assert!(
                head.starts_with("HTTP/1.1 503 Service Unavailable"),
                "{cut:?}: {head}"
            );
            assert!(
                body.is_null(),
                "{cut:?}: uncertain durability invents no IDs"
            );
        }
        let mut durable = fixture.records();
        let (agent, goal) = durable
            .iter()
            .find_map(|record| match record {
                Record::AgentHired {
                    id,
                    browser_onboarding: Some(meta),
                    ..
                } => Some((*id, meta.intended_goal.id)),
                _ => None,
            })
            .expect("hire fsynced before cut");
        if cut == OnboardingFailurePoint::TurnIssued {
            assert!(head.starts_with("HTTP/1.1 200 OK"), "{cut:?}: {head}");
            assert_eq!(
                body,
                json!({"attempt_id":request["attempt_id"], "state":"complete", "agent":agent, "goal":goal})
            );
            issued_onboarding_fence(&fixture, &running.handle, agent, goal);
            durable = fixture.records();
            let (retry_head, retry) = post_onboard(addr, &cookie, &csrf, &request);
            assert!(retry_head.starts_with("HTTP/1.1 200 OK"), "{retry_head}");
            assert_eq!(retry, body);
            let mut fresh = request.clone();
            fresh["attempt_id"] = json!(format!("{}E", "A".repeat(42)));
            let (fresh_head, fresh_body) = post_onboard(addr, &cookie, &csrf, &fresh);
            assert!(
                fresh_head.starts_with("HTTP/1.1 503 Service Unavailable"),
                "{fresh_head}"
            );
            assert!(fresh_body.is_null());
            assert_eq!(
                fixture.records(),
                durable,
                "indexed retry and fresh refusal append nothing"
            );
            let shown = snapshot_optional(addr, &cookie, &csrf);
            assert!(
                shown
                    .as_ref()
                    .is_none_or(|(frame, _)| frame.get("onboarding").is_none())
            );
        }
        let counts = |records: &[Record]| {
            (
                records
                    .iter()
                    .filter(|record| {
                        matches!(
                            record,
                            Record::AgentHired {
                                browser_onboarding: Some(_),
                                ..
                            }
                        )
                    })
                    .count(),
                records
                    .iter()
                    .filter(|record| {
                        matches!(
                            record,
                            Record::GoalSet {
                                browser_onboarding: Some(_),
                                ..
                            }
                        )
                    })
                    .count(),
                records
                    .iter()
                    .filter(|record| matches!(record, Record::TurnIssued { .. }))
                    .count(),
                records
                    .iter()
                    .filter(|record| matches!(record, Record::TurnSpawned { .. }))
                    .count(),
            )
        };
        assert_eq!(
            counts(&durable),
            match cut {
                OnboardingFailurePoint::Hire | OnboardingFailurePoint::HireProjection =>
                    (1, 0, 0, 0),
                OnboardingFailurePoint::Goal => (1, 1, 0, 0),
                OnboardingFailurePoint::TurnIssued => (1, 1, 1, 0),
                OnboardingFailurePoint::LocalGoal
                | OnboardingFailurePoint::LocalRetire
                | OnboardingFailurePoint::Answer
                | OnboardingFailurePoint::Control
                | OnboardingFailurePoint::GoalChange
                | OnboardingFailurePoint::Rename
                | OnboardingFailurePoint::Look
                | OnboardingFailurePoint::Retire
                | OnboardingFailurePoint::Ticket
                | OnboardingFailurePoint::ClosingAnnounced
                | OnboardingFailurePoint::ClosingAck
                | OnboardingFailurePoint::ClosingComplete
                | OnboardingFailurePoint::SpendRefresh => {
                    unreachable!(
                        "local, answer, control, goal and retire cuts are tested separately"
                    )
                }
            }
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

        let (parts, host) = native_fixture_seams(&fixture, fake_seams(&settings));
        let daemon = boot(&settings, parts).expect("replay durable prefix");
        let addr = daemon.floor_addr().expect("replayed Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        if cut == OnboardingFailurePoint::TurnIssued {
            recover_owed_browser_turn(&settings, &fixture, &running, agent, goal);
        }
        if matches!(
            cut,
            OnboardingFailurePoint::Goal | OnboardingFailurePoint::TurnIssued
        ) {
            issued_goal_turn(&fixture, goal);
            running.handle.doctor().unwrap();
        }
        let before_retry = fixture.records();
        let (_, complete) = post_onboard(addr, &cookie, &csrf, &request);
        assert_eq!(complete["state"], "complete", "{cut:?}");
        assert_eq!(complete["agent"], agent.to_string());
        assert_eq!(complete["goal"], goal.to_string());
        issued_goal_turn(&fixture, goal);
        running.handle.doctor().unwrap();
        let after_retry = fixture.records();
        assert_eq!(counts(&after_retry).0, 1, "{cut:?}: no rehire");
        assert_eq!(counts(&after_retry).1, 1, "{cut:?}: no second initial goal");
        if !matches!(
            cut,
            OnboardingFailurePoint::Hire | OnboardingFailurePoint::HireProjection
        ) {
            assert_eq!(
                counts(&after_retry).2,
                counts(&before_retry).2,
                "{cut:?}: retry did not issue a new turn"
            );
        }
        assert_eq!(
            counts(&after_retry).3,
            1,
            "{cut:?}: one host spawn after recovery"
        );
        assert_eq!(host.spawned().len(), 1, "{cut:?}: one host launch");
        if cut == OnboardingFailurePoint::TurnIssued {
            let turn = nth_turn(&fixture, goal, 0);
            let task = |records: &[Record]| {
                records
                    .iter()
                    .find_map(|record| match record {
                        Record::TurnIssued {
                            turn: seen, task, ..
                        } if *seen == turn => Some(task.clone()),
                        _ => None,
                    })
                    .unwrap()
            };
            assert_eq!(task(&after_retry), task(&durable));
            assert_eq!(managed_turns(&fixture, goal), 1);
        }
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

#[test]
fn pre_append_failure_leaves_no_hire_and_same_key_retries_after_restart() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
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
    .expect("first boot");
    daemon
        .append_failure()
        .fail_nth(NonZeroUsize::new(1).expect("one"));
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let request = json!({
        "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
    });
    let (head, body) = post_onboard(addr, &cookie, &csrf, &request);
    assert!(
        head.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{head}"
    );
    assert!(body.is_null());
    assert!(!fixture.records().iter().any(|record| matches!(
        record,
        Record::AgentHired {
            browser_onboarding: Some(_),
            ..
        }
    )));
    let (same_run_head, same_run) = post_onboard(addr, &cookie, &csrf, &request);
    assert!(
        same_run_head.starts_with("HTTP/1.1 200 OK"),
        "a known no-effect 503 can be retried in this run: {same_run_head}"
    );
    assert_eq!(same_run["state"], "complete");
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                Record::AgentHired {
                    browser_onboarding: Some(_),
                    ..
                }
            ))
            .count(),
        1,
        "known no-effect retry creates exactly one hire"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

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
    .expect("restart after no-effect cut");
    let addr = daemon.floor_addr().expect("new Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, complete) = post_onboard(addr, &cookie, &csrf, &request);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(complete["state"], "complete");
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                Record::AgentHired {
                    browser_onboarding: Some(_),
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn post_fsync_uncertainty_fences_fresh_hires_and_catalog() {
    for cut in [
        OnboardingFailurePoint::Hire,
        OnboardingFailurePoint::HireProjection,
        OnboardingFailurePoint::Goal,
        OnboardingFailurePoint::TurnIssued,
    ] {
        let fixture = Fixture::new();
        let settings = enrolled_settings(&fixture);
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
        .expect("boot Floor");
        let fault = daemon.onboarding_failure();
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let mut request = json!({
            "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
        });
        for last in ["A", "E", "I"] {
            request["attempt_id"] = json!(format!("{}{}", "A".repeat(42), last));
            let (head, complete) = post_onboard(addr, &cookie, &csrf, &request);
            assert!(head.starts_with("HTTP/1.1 200 OK"), "{cut:?}: {head}");
            let agent = AgentId::parse(complete["agent"].as_str().unwrap()).unwrap();
            await_continuation_outcome(&fixture, &running.handle, agent);
        }
        let (before, _) = snapshot(addr, &cookie, &csrf);
        assert!(
            before["onboarding"].is_object(),
            "three hires leave a catalog"
        );
        request["attempt_id"] = json!(format!("{}M", "A".repeat(42)));
        fault.arm(cut);
        let (head, body) = post_onboard(addr, &cookie, &csrf, &request);
        if cut == OnboardingFailurePoint::TurnIssued {
            let (agent, goal) = completed_cut_reply(&fixture, &request, &head, &body);
            issued_onboarding_fence(&fixture, &running.handle, agent, goal);
        } else {
            assert!(
                head.starts_with("HTTP/1.1 503 Service Unavailable"),
                "{cut:?}: {head}"
            );
            assert!(body.is_null());
        }
        assert_eq!(
            onboarding_hire_count(&fixture.records()),
            4,
            "{cut:?}: the fourth hire is durable"
        );
        let (head, pending) = post_onboard(addr, &cookie, &csrf, &request);
        if cut == OnboardingFailurePoint::TurnIssued {
            assert!(head.starts_with("HTTP/1.1 200 OK"), "{cut:?}: {head}");
            assert_eq!(pending, body);
        } else {
            assert!(head.starts_with("HTTP/1.1 202 Accepted"), "{cut:?}: {head}");
            assert_eq!(pending["state"], "pending");
        }
        let after = snapshot_optional(addr, &cookie, &csrf);
        assert!(
            after
                .as_ref()
                .is_none_or(|(frame, _)| frame.get("onboarding").is_none()),
            "{cut:?}: uncertain truth hides catalog"
        );
        request["attempt_id"] = json!(format!("{}Q", "A".repeat(42)));
        let (head, body) = post_onboard(addr, &cookie, &csrf, &request);
        assert!(
            head.starts_with("HTTP/1.1 503 Service Unavailable"),
            "{cut:?}: {head}"
        );
        assert!(body.is_null());
        let (again, again_body) = post_onboard(addr, &cookie, &csrf, &request);
        assert!(
            again.starts_with("HTTP/1.1 503 Service Unavailable"),
            "{cut:?}: a new request fenced before append remains retryable 503: {again}"
        );
        assert!(again_body.is_null());
        assert_eq!(
            onboarding_hire_count(&fixture.records()),
            4,
            "{cut:?}: no fifth durable hire"
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

/// A workspace is offered while any of its checks can run, not only its own first one: a hire
/// there names the check it uses, and a chosen check that cannot run is refused when it runs.
///
/// Mutation: reading only the first check omits the workspace.
#[test]
fn a_workspace_whose_first_check_cannot_run_is_still_offered() {
    let fixture = Fixture::new();
    let mut settings = enrolled_settings(&fixture);
    let profile = &mut settings.enrollment.profiles[0];
    profile.checks.insert(
        0,
        duded::floor_actions::EnrolledCheck::enrolled(
            "Checks pass",
            Predicate::Command {
                argv: vec!["/nonexistent/check-program".into()],
                timeout_s: 10,
            },
        ),
    );
    profile.checks[1].id = "suite".into();
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (frame, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(
        frame["onboarding"]["workspaces"][0]["workspace_id"], "repo_1",
        "{frame}"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn snapshot_omits_invalid_profile_predicate_and_token_reserve() {
    let fixture = Fixture::new();
    for case in ["zero predicate timeout", "zero token reserve"] {
        let mut settings = enrolled_settings(&fixture);
        let profile = &mut settings.enrollment.profiles[0];
        match case {
            "zero predicate timeout" => {
                profile.checks[0].predicate = Predicate::Command {
                    argv: vec![true_program().into()],
                    timeout_s: 0,
                }
            }
            "zero token reserve" => {
                profile.agent_budget.tokens_per_day = Some(dudes_types::Tokens::new(10));
            }
            _ => unreachable!(),
        }
        if case == "zero token reserve" {
            settings.per_turn_floor.tokens = dudes_types::Tokens::ZERO;
        }
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
        .expect("boot Floor");
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let (frame, _) = snapshot(addr, &cookie, &csrf);
        assert!(
            frame.get("onboarding").is_none(),
            "{case}: invalid profile is omitted"
        );
        let request = json!({
            "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
        });
        let (head, body) = post_onboard(addr, &cookie, &csrf, &request);
        assert!(head.starts_with("HTTP/1.1 403 Forbidden"), "{case}: {head}");
        assert!(body.is_null());
        assert!(
            !fixture.records().iter().any(|record| matches!(
                record,
                Record::AgentHired {
                    browser_onboarding: Some(_),
                    ..
                }
            )),
            "{case}: invalid profile appends nothing"
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
    let mut zero_dollar_reserve = enrolled_settings(&fixture);
    zero_dollar_reserve.per_turn_floor.usd = Usd::ZERO;
    assert!(
        boot(
            &zero_dollar_reserve,
            seams(
                &zero_dollar_reserve,
                SharedHost::new(),
                Arc::new(ScriptedRunner::default()),
                Arc::new(TestClock::new()),
                Witness::default()
            )
        )
        .is_err(),
        "a zero dollar reserve beside a floor ceiling refuses boot before any catalog can be \
         served"
    );
}

#[cfg(unix)]
#[test]
fn fresh_snapshot_omits_predicate_that_disappeared_after_enrollment() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = Fixture::new();
    let mut settings = enrolled_settings(&fixture);
    let program = fixture.state.path().join("predicate-program");
    std::fs::write(&program, b"stub").expect("predicate program");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
        .expect("executable predicate");
    settings.enrollment.profiles[0].checks[0].predicate = Predicate::Command {
        argv: vec![program.to_string_lossy().into_owned()],
        timeout_s: 10,
    };
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
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    assert!(
        snapshot(addr, &cookie, &csrf).0["onboarding"].is_object(),
        "positive control: the existing typed predicate is eligible"
    );
    std::fs::remove_file(&program).expect("predicate drift");
    let (frame, _) = snapshot(addr, &cookie, &csrf);
    assert!(
        frame.get("onboarding").is_none(),
        "missing predicate removes its profile"
    );
    let request = json!({
        "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
    });
    let (head, body) = post_onboard(addr, &cookie, &csrf, &request);
    assert!(head.starts_with("HTTP/1.1 403 Forbidden"), "{head}");
    assert!(body.is_null());
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn fourth_durable_hire_precedes_queued_session_catalog_snapshot() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
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
    .expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let mut request = json!({
        "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
    });
    for last in ["A", "E", "I"] {
        request["attempt_id"] = json!(format!("{}{}", "A".repeat(42), last));
        let (head, _) = post_onboard(addr, &cookie, &csrf, &request);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    }
    assert!(
        snapshot(addr, &cookie, &csrf).0["onboarding"].is_object(),
        "three durable hires still show one available slot"
    );
    pause.arm(OnboardingPausePoint::BetweenEffects);
    request["attempt_id"] = json!(format!("{}M", "A".repeat(42)));
    let owner_request = request.clone();
    let owner_cookie = cookie.clone();
    let owner_csrf = csrf.clone();
    let owner =
        std::thread::spawn(move || post_onboard(addr, &owner_cookie, &owner_csrf, &owner_request));
    assert!(
        pause.wait_reached(Duration::from_secs(3)),
        "fourth hire folded before its goal"
    );
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                Record::AgentHired {
                    browser_onboarding: Some(_),
                    ..
                }
            ))
            .count(),
        4
    );
    let (sent_tx, sent_rx) = std::sync::mpsc::channel();
    let read_cookie = cookie.clone();
    let read_csrf = csrf.clone();
    let queued_snapshot = std::thread::spawn(move || {
        snapshot_optional_signaled(addr, &read_cookie, &read_csrf, Some(sent_tx))
    });
    sent_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("snapshot subscribe sent while writer held");
    pause.release();
    let (head, complete) = owner.join().expect("fourth Hire response");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(complete["state"], "complete");
    let (frame, _) = queued_snapshot
        .join()
        .expect("queued snapshot result")
        .expect("authenticated snapshot");
    assert!(
        frame.get("onboarding").is_none(),
        "the fourth committed hire omits the sibling"
    );
    request["attempt_id"] = json!(format!("{}Q", "A".repeat(42)));
    let (head, _) = post_onboard(addr, &cookie, &csrf, &request);
    assert!(
        head.starts_with("HTTP/1.1 403 Forbidden"),
        "stale catalog cannot admit a fifth hire: {head}"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// One operator call to a current method on a fresh connection.
fn operator_current(
    settings: &duded::Settings,
    method: dudes_proto::rpc::CurrentMethod,
    params: Option<Value>,
) -> Value {
    let credential = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("operator credential");
    let mut client = Client::connect(settings.socket()).expect("daemon socket");
    client
        .call_current_as_operator(
            method,
            params,
            OperatorCredential::new(credential).expect("credential"),
        )
        .expect("the daemon answers")
        .result
        .expect("the operator is admitted")
}

/// A browser's session carries its hire count across `dudes upgrade`: the count is rebuilt from the
/// journal by the digest the imported session keeps, so four hires before the upgrade are still four
/// after it, and a session the upgrade did not carry starts from none.
#[test]
fn an_imported_session_keeps_its_browser_hire_count_from_the_journal() {
    use dudes_proto::rpc::CurrentMethod;

    let fixture = Fixture::new();
    let mut settings = enrolled_settings(&fixture);
    if let Some(floor) = settings.floor.as_mut() {
        floor.port = None;
    }
    let clock = Arc::new(TestClock::new());
    let boot_run = |clock: &Arc<TestClock>| {
        boot(
            &settings,
            seams(
                &settings,
                SharedHost::new(),
                Arc::new(ScriptedRunner::default()),
                Arc::clone(clock),
                Witness::default(),
            ),
        )
        .expect("boot Floor")
    };
    let hire = |addr: SocketAddr, cookie: &str, csrf: &str, last: &str| {
        let request = json!({
            "attempt_id": format!("{}{last}", "A".repeat(42)),
            "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
        });
        post_onboard(addr, cookie, csrf, &request).0
    };

    let daemon = boot_run(&clock);
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    for last in ["A", "E", "I", "M"] {
        let head = hire(addr, &cookie, &csrf, last);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        if last == "A" {
            assert!(
                snapshot(addr, &cookie, &csrf).0["onboarding"].is_object(),
                "control: a session with one hire is still offered another"
            );
        }
    }
    let head = hire(addr, &cookie, &csrf, "Q");
    assert!(
        head.starts_with("HTTP/1.1 403 Forbidden"),
        "control: a session that hired four times is refused a fifth: {head}"
    );
    let exported = operator_current(&settings, CurrentMethod::FloorSessionExport, None);
    // Stop only once the journal has stopped growing, so the next run replays a settled history.
    let mut seen = fixture.records().len();
    let mut quiet = 0;
    while quiet < 4 {
        std::thread::sleep(Duration::from_millis(150));
        let now = fixture.records().len();
        quiet = if now == seen { quiet + 1 } else { 0 };
        seen = now;
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let daemon = boot_run(&clock);
    assert_eq!(
        daemon.floor_addr(),
        Some(addr),
        "the Floor kept its address"
    );
    let mut running = common::start(daemon);
    let imported = operator_current(
        &settings,
        CurrentMethod::FloorSessionImport,
        Some(json!({ "exported": exported })),
    );
    assert_eq!(imported, json!({ "restored": 1 }));
    let recovered = raw_recover(addr, &cookie);
    let rotated = serde_json::from_str::<Value>(recovered.1.trim()).expect("recovery JSON")["csrf"]
        .as_str()
        .expect("rotated CSRF")
        .to_owned();
    let head = hire(addr, &cookie, &rotated, "U");
    assert!(
        head.starts_with("HTTP/1.1 403 Forbidden"),
        "the four hires before the upgrade still count: {head}"
    );
    assert!(
        snapshot(addr, &cookie, &rotated)
            .0
            .get("onboarding")
            .is_none(),
        "the carried session is offered no further hire"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// `POST /floor/session/recover` for `cookie`, as the status head and the body.
fn raw_recover(addr: SocketAddr, cookie: &str) -> (String, String) {
    let host = addr.to_string();
    let mut stream = TcpStream::connect(addr).expect("Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    let request = format!(
        "POST /floor/session/recover HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: dudes_floor_session={cookie}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).expect("recover");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("a complete response");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    (head.to_owned(), body.to_owned())
}

/// The browser's first hire ends the pre-agent operator epoch exactly as a local hire does.
///
/// Every local connection here resolves `NoRecordedAgent` and presents the operator credential,
/// which is also what an agent child that escaped its ancestry and read the credential presents.
/// The one accepted before the hire keeps the lease it acquired then, and its `agent.steer`
/// applies: the control, run first so that a red subject cannot hide a dead control. The subject
/// is accepted while the writer is held just after the hire's `AgentHired` fsync, the earliest
/// moment the hire is durable, so a seal that came any later than the append would let it
/// acquire. It must acquire nothing, so the same call is suppressed.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one daemon carries the pre-hire control, the fsync-time acceptance and the refusal"
)]
fn browser_onboarding_hire_seals_the_pre_agent_epoch() {
    use dudes_proto::rpc::{ErrorCode, RefusalGate, RefusedData};
    use dudes_proto::{ConditionedMethod, ConditionedTarget, PeerProvenance};

    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
    let column = common::ScriptedColumn::new(PeerProvenance::NoRecordedAgent);
    let daemon = boot(
        &settings,
        common::seams_with(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
            Arc::clone(&column) as Arc<dyn dudes_rpc::PeerResolver>,
        ),
    )
    .expect("boot Floor on a fresh state directory");
    let authority = Arc::clone(daemon.operator_authority());
    assert!(
        !authority.is_sealed(),
        "zero replayed agents opens the pre-agent epoch"
    );
    let pause = daemon.onboarding_pause();
    pause.arm(OnboardingPausePoint::AfterHireFsync);
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let credential = || {
        OperatorCredential::new(
            std::fs::read_to_string(
                settings
                    .runtime_dir
                    .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
            )
            .expect("operator credential"),
        )
        .expect("credential")
    };

    let mut pre_hire = Client::connect(settings.socket()).expect("connect before the hire");
    // Acquisition precedes a connection's first request, so an answered ping means it ran.
    let ping = pre_hire
        .call(Method::SystemPing, None)
        .expect("pre-hire ping");
    assert!(ping.error.is_none(), "system.ping: {ping:?}");
    assert_eq!(
        authority.live_leases(),
        1,
        "the pre-hire connection holds a lease"
    );

    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let hire = std::thread::spawn(move || {
        post_onboard(
            addr,
            &cookie,
            &csrf,
            &json!({
                "attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                "workspace_id":"repo_1", "name":"Kim", "role":"engineer", "goal_text":"ship"
            }),
        )
    });
    assert!(
        pause.wait_reached(Duration::from_secs(3)),
        "the writer reached the hire's fsync"
    );
    let hires: Vec<AgentId> = fixture
        .records()
        .iter()
        .filter_map(|record| match record {
            Record::AgentHired { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    let sealed_after_hire = authority.is_sealed();
    let mut post_hire = Client::connect(settings.socket()).expect("connect after the hire");
    let ping = post_hire
        .call(Method::SystemPing, None)
        .expect("post-hire ping");
    assert!(ping.error.is_none(), "system.ping: {ping:?}");
    let leases_after_hire = authority.live_leases();
    pause.release();
    let (head, response) = hire.join().expect("the onboarding request thread");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    let agent: AgentId =
        serde_json::from_value(response["agent"].clone()).expect("the browser-hired AgentId");
    assert_eq!(
        hires,
        [agent],
        "the browser hire is this state directory's first AgentHired, and it was durable"
    );

    let applied = pre_hire
        .call_as_operator(
            Method::AgentSteer,
            Some(json!({ "agent": agent, "text": "pre-hire lease" })),
            credential(),
        )
        .expect("the pre-hire connection is answered");
    assert!(
        applied.error.is_none(),
        "control: a lease held from before the hire applies: {applied:?}"
    );
    assert!(
        fixture.records().iter().any(|record| matches!(
            record,
            Record::Steer { agent: who, text, provenance: Some(PeerProvenance::NoRecordedAgent) }
                if *who == agent && text == "pre-hire lease"
        )),
        "control: the pre-hire steer is journaled"
    );

    let denied = post_hire
        .call_as_operator(
            Method::AgentSteer,
            Some(json!({ "agent": agent, "text": "post-hire local" })),
            credential(),
        )
        .expect("the post-hire connection is answered");
    let records = fixture.records();
    let steered = records.iter().any(|record| {
        matches!(
            record,
            Record::Steer { agent: who, text, .. } if *who == agent && text == "post-hire local"
        )
    });
    let suppressed = records
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::ConditionedEffectSuppressed {
                    method: ConditionedMethod::AgentSteer,
                    target: ConditionedTarget::Agent { agent: who },
                    provenance: PeerProvenance::NoRecordedAgent,
                    reason,
                    ..
                } if *who == agent && reason == "operator authority unavailable"
            )
        })
        .count();
    let error = denied.error.as_ref().unwrap_or_else(|| {
        panic!(
            "the post-hire connection's agent.steer applied (Steer journaled: {steered}; at the \
             hire's fsync: sealed {sealed_after_hire}, live leases {leases_after_hire}): \
             {denied:?}"
        )
    });
    assert_eq!(error.code, ErrorCode::Refused.code(), "{denied:?}");
    let data: RefusedData =
        serde_json::from_value(error.data.clone().expect("typed refusal")).expect("RefusedData");
    assert_eq!(data.gate, RefusalGate::ConditionedEffect);
    assert_eq!(
        suppressed, 1,
        "one ConditionedEffectSuppressed is journaled"
    );
    assert!(!steered, "the suppressed steer never became a Steer");
    assert!(
        sealed_after_hire,
        "the browser hire sealed the epoch by the time its AgentHired was durable"
    );
    assert_eq!(
        column.asked(),
        3,
        "the pre-hire, session-issue and post-hire connections each resolved once"
    );
    drop(post_hire);
    drop(pre_hire);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

// ---- the Floor answer ----------------------------------------------------------------
//
// One opt-in POST closes one open question named by `(agent, question_lsn)`, and the answer rides
// the agent's next spawned Managed turn. Every test below reads the journal from disk, so what it
// asserts is what a replay would rebuild.

/// Where the stub child writes the task it received on standard input.
const TASK_CAPTURE: &str = "DUDES_FLOOR_ANSWER_TASK_CAPTURE";

/// Floor settings with answering on or off; nothing else differs from the fixture's.
fn answer_settings(fixture: &Fixture, enabled: bool) -> duded::Settings {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.floor_answer_enabled = enabled;
    settings
}

/// A canonical request identity: 32 bytes whose first is `4 * k`, the rest zero.
fn request_id(k: usize) -> String {
    const LEADING: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    format!("{}{}", char::from(LEADING[k]), "A".repeat(42))
}

fn answer_body(request_id: &str, agent: AgentId, question: Lsn, text: &str) -> Value {
    json!({
        "request_id": request_id,
        "agent": agent,
        "question_lsn": question.0.to_string(),
        "text": text,
    })
}

fn post_bytes(
    addr: SocketAddr,
    path: &str,
    cookie: &str,
    csrf: &str,
    body: &[u8],
    sent: Option<mpsc::Sender<()>>,
) -> (String, Value) {
    let host = addr.to_string();
    let mut stream = TcpStream::connect(addr).expect("Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: dudes_floor_session={cookie}\r\nX-Dudes-CSRF: {csrf}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("request head");
    stream.write_all(body).expect("request body");
    if let Some(sent) = sent {
        sent.send(()).expect("request sent signal");
    }
    let head = read_head(&mut stream);
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .expect("response length")
        .parse()
        .expect("decimal length");
    let mut body = vec![0; length];
    stream.read_exact(&mut body).expect("response body");
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).expect("response JSON")
    };
    (head, value)
}

fn post_answer(addr: SocketAddr, cookie: &str, csrf: &str, body: &Value) -> (String, Value) {
    post_bytes(
        addr,
        "/floor/answer",
        cookie,
        csrf,
        &serde_json::to_vec(body).expect("answer JSON"),
        None,
    )
}

/// The position of `agent`'s latest `HumanQuestion` in the journal on disk.
fn question_lsn(fixture: &Fixture, agent: AgentId) -> Lsn {
    fixture
        .entries()
        .into_iter()
        .rev()
        .find_map(|(lsn, entry)| match entry.record {
            Record::HumanQuestion { agent: who, .. } if who == agent => Some(lsn),
            _ => None,
        })
        .expect("the agent asked")
}

/// Hire through the loop, open a question, set a goal: the human gate holds it `Waiting`, with no turn.
fn waiting_agent(running: &common::Running, fixture: &Fixture, question: &str) -> (AgentId, Lsn) {
    let agent = common::hire(&running.handle, fixture.state.path());
    running
        .handle
        .ask(Ask::Ask {
            agent,
            text: question.to_owned(),
        })
        .expect("the question");
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[true_program()]),
    );
    (agent, question_lsn(fixture, agent))
}

fn answers_for(records: &[Record], agent: AgentId) -> usize {
    records
        .iter()
        .filter(|record| matches!(record, Record::HumanAnswer { agent: who, .. } if *who == agent))
        .count()
}

fn empty_refusal(head: &str, body: &Value, status: &str) {
    assert!(head.starts_with(status), "{head}");
    assert!(
        body.is_null(),
        "an empty refusal carries no envelope: {body}"
    );
}

/// The request digest, recomputed here rather than taken from the daemon.
fn expected_digest(agent: AgentId, question: Lsn, text: &str) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"dudes.answer.v1\0");
    for (field, value) in [
        ("agent", agent.to_string()),
        ("question_lsn", question.0.to_string()),
        ("text", text.to_owned()),
    ] {
        hasher.update(u16::try_from(field.len()).unwrap().to_be_bytes());
        hasher.update(field.as_bytes());
        hasher.update(u32::try_from(value.len()).unwrap().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.finalize().into()
}

fn answer_instruction_lineage(
    record: &Record,
    agent: AgentId,
    turn: TurnId,
    started: Lsn,
    spawned: Lsn,
) {
    let owner = match record {
        Record::InstructionObservationStarted { owner, .. } => owner,
        Record::InstructionObservationFinished {
            owner, baseline, ..
        } => {
            assert_eq!(
                *baseline,
                dudes_proto::InstructionBaselineRef::Started { lsn: started }
            );
            owner
        }
        Record::InstructionAttemptBoundarySettled {
            owner,
            started_lsn,
            proof,
        } => {
            assert_eq!(*started_lsn, started);
            assert_eq!(
                *proof,
                dudes_proto::InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                    receipt: dudes_proto::InstructionNativeBoundaryReceipt::Spawned {
                        lsn: spawned
                    },
                }
            );
            owner
        }
        _ => return,
    };
    assert!(
        matches!(owner, dudes_proto::InstructionObservationOwner::Managed { agent: seen_agent, turn: seen_turn, .. }
        if *seen_agent == agent && *seen_turn == turn)
    );
}

/// The primary test, stub-backed and read from the replayed journal.
///
/// One attributed `HumanAnswer` — `provenance: None` beside `browser_answer: Some` — then a
/// `LoopDecided` and a `TurnIssued` for the same agent whose `answers` names it and whose task
/// carries its bytes; the stub child received those bytes on its standard input; no `pty.write`,
/// steer or conditioned disposition exists. Every record from the answer through the stub turn is
/// one of a named set of kinds and names that one agent.
///
/// Mutants that must turn this red: the fold withholds pending answers from `LoopInput`;
/// `issue_turn` lists no answers; the machine stamps the answer with a provenance.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end stub scenario asserts the whole durable chain and its allowlist"
)]
fn browser_answer_is_finite_bound_and_observed() {
    let fixture = Fixture::new();
    let capture = tempfile::tempdir().expect("task capture directory");
    let mut settings = answer_settings(&fixture, true);
    settings.daemon_env.push(EnvVar::new(HELPER_ROLE, "1"));
    settings
        .daemon_env
        .push(EnvVar::new(TASK_CAPTURE, capture.path().as_os_str()));
    let daemon = boot(&settings, real_stub_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (agent, question) = waiting_agent(&running, &fixture, "which branch?");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);

    let (snapshot, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(snapshot["answer"], json!({ "max_text_bytes": 4096 }));
    let row = snapshot["projection"]["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .find(|row| row["id"] == json!(agent))
        .expect("the waiting agent")
        .clone();
    assert_eq!(row["status"], "waiting");
    assert_eq!(row["question_lsn"], json!(question.0.to_string()));
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::TurnIssued { .. })),
        "control: no continuation exists before the answer"
    );

    let text = "use main,\n\tthen rebase";
    let wire = request_id(1);
    let (head, response) = post_answer(
        addr,
        &cookie,
        &csrf,
        &answer_body(&wire, agent, question, &format!("  {text}\r\n")),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(head.contains("Cache-Control: no-store"));
    assert!(!head.to_ascii_lowercase().contains("access-control-allow"));
    assert_eq!(
        response,
        json!({
            "request_id": wire, "state": "answered", "agent": agent,
            "question_lsn": question.0.to_string(),
        })
    );
    fixture.await_record("the stub turn's decision after its end", |record| {
        matches!(
            record,
            Record::LoopDecided { agent: who, decision: LoopDecision::Done, .. } if *who == agent
        )
        .then_some(())
    });
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let entries = fixture.entries();
    let records: Vec<Record> = entries.iter().map(|(_, e)| e.record.clone()).collect();
    assert_eq!(answers_for(&records, agent), 1, "one HumanAnswer");
    let (answered_at, attribution) = entries
        .iter()
        .find_map(|(lsn, entry)| match &entry.record {
            Record::HumanAnswer {
                agent: who,
                text: journaled,
                provenance,
                browser_answer,
            } if *who == agent => {
                assert_eq!(journaled, text, "trimmed, and otherwise verbatim");
                assert_eq!(*provenance, None, "browser-attributed, never Some");
                Some((*lsn, browser_answer.clone().expect("attributed")))
            }
            _ => None,
        })
        .expect("the answer");
    assert_eq!(attribution.question_lsn, question);
    let mut decoded_id = [0_u8; 32];
    decoded_id[0] = 4;
    assert_eq!(attribution.request_id.bytes(), decoded_id);
    assert_eq!(
        attribution.request_digest.bytes(),
        expected_digest(agent, question, text)
    );
    assert_eq!(
        attribution.session_digest.bytes(),
        <[u8; 32]>::from(<sha2::Sha256 as sha2::Digest>::digest(cookie.as_bytes()))
    );

    let after: Vec<&(Lsn, dudes_proto::Entry)> = entries
        .iter()
        .filter(|(lsn, _)| *lsn >= answered_at)
        .collect();
    let decided = after
        .iter()
        .position(
            |(_, e)| matches!(&e.record, Record::LoopDecided { agent: who, .. } if *who == agent),
        )
        .expect("a later LoopDecided");
    let (turn, task) = after[decided..]
        .iter()
        .find_map(|(_, e)| match &e.record {
            Record::TurnIssued {
                agent: who,
                turn,
                task,
                answers,
                ..
            } if *who == agent => {
                assert_eq!(answers, &vec![answered_at], "the turn names the answer");
                Some((*turn, task.clone()))
            }
            _ => None,
        })
        .expect("a TurnIssued after the decision");
    assert!(task.contains(text), "{task}");
    assert!(after.iter().any(|(_, e)| matches!(
        &e.record,
        Record::TurnSpawned { turn: t, .. } if *t == turn
    )));
    let started = after
        .iter()
        .find_map(|(lsn, entry)| match &entry.record {
            Record::InstructionObservationStarted {
                owner: dudes_proto::InstructionObservationOwner::Managed { turn: seen, .. },
                ..
            } if *seen == turn => Some(*lsn),
            _ => None,
        })
        .unwrap();
    let spawned = after
        .iter()
        .find_map(|(lsn, entry)| {
            matches!(entry.record, Record::TurnSpawned { turn: seen, .. } if seen == turn)
                .then_some(*lsn)
        })
        .unwrap();

    let allowed = [
        RecordKind::HumanAnswer,
        RecordKind::LoopDecided,
        RecordKind::TurnIssued,
        RecordKind::HookTokenMinted,
        RecordKind::TurnSpawned,
        RecordKind::SessionLearned,
        RecordKind::TurnEnded,
        RecordKind::PredicateResult,
        RecordKind::InstructionObservationStarted,
        RecordKind::InstructionObservationFinished,
        RecordKind::InstructionAttemptBoundarySettled,
    ];
    let goal = records
        .iter()
        .find_map(|record| match record {
            Record::GoalSet {
                agent: who, goal, ..
            } if *who == agent => Some(goal.id),
            _ => None,
        })
        .expect("the agent's goal");
    for (lsn, entry) in &after {
        assert!(
            allowed.contains(&entry.record.kind()),
            "{lsn}: {:?} is outside the answer's allowlist",
            entry.record.kind()
        );
        answer_instruction_lineage(&entry.record, agent, turn, started, spawned);
        let json = serde_json::to_value(&entry.record).expect("record JSON");
        let named = [
            &json["agent"],
            &json["id"],
            &json["goal"],
            &json["owner"]["agent"],
        ]
        .into_iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
        assert!(
            !named.is_empty()
                && named
                    .iter()
                    .all(|id| *id == agent.to_string() || *id == goal.to_string()),
            "{lsn}: every record names the one answered agent or its goal: {json}"
        );
    }

    for entry in &records {
        let json = serde_json::to_string(entry).expect("record JSON");
        if json.contains("then rebase") {
            assert!(
                matches!(
                    entry.kind(),
                    RecordKind::HumanAnswer | RecordKind::LoopDecided | RecordKind::TurnIssued
                ),
                "the answer bytes reached a {:?}",
                entry.kind()
            );
        }
        assert!(!matches!(
            entry.kind(),
            RecordKind::PtyWriteRequested
                | RecordKind::PtyWriteCompleted
                | RecordKind::Steer
                | RecordKind::SteerConsumed
                | RecordKind::ConditionedEffectSuppressed
        ));
    }
    let journal = serde_json::to_string(&entries).expect("journal JSON");
    for secret in [cookie.as_str(), csrf.as_str(), wire.as_str()] {
        assert!(
            !journal.contains(secret),
            "raw browser credential in the journal"
        );
    }

    let delivered: Vec<String> = std::fs::read_dir(capture.path())
        .expect("capture directory")
        .map(|file| std::fs::read_to_string(file.expect("capture").path()).expect("task"))
        .collect();
    assert!(
        delivered.iter().any(|task| task.contains(text)),
        "the stub child received the answer on its standard input: {delivered:?}"
    );
}

/// Every browser credential is checked before any effect: wrong Host, wrong or missing Origin, an
/// unknown cookie, a wrong or missing CSRF and an expired session each get the uniform empty
/// `403`, and the journal does not move. Control: the right credentials answer.
#[test]
fn browser_answer_refuses_wrong_session_host_origin_and_csrf_without_effect() {
    let fixture = Fixture::new();
    let settings = answer_settings(&fixture, true);
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (agent, question) = waiting_agent(&running, &fixture, "which branch?");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = serde_json::to_vec(&answer_body(&request_id(1), agent, question, "main")).unwrap();
    let host = addr.to_string();
    let request = |host: &str, origin: Option<&str>, cookie: &str, csrf: Option<&str>| {
        let mut head = format!("POST /floor/answer HTTP/1.1\r\nHost: {host}\r\n");
        if let Some(origin) = origin {
            let _ = write!(head, "Origin: {origin}\r\n");
        }
        let _ = write!(head, "Cookie: dudes_floor_session={cookie}\r\n");
        if let Some(csrf) = csrf {
            let _ = write!(head, "X-Dudes-CSRF: {csrf}\r\n");
        }
        let _ = write!(
            head,
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&body);
        bytes
    };
    let origin = format!("http://{host}");
    let unknown = "A".repeat(43);
    let before = fixture.records();
    for (label, bytes) in [
        (
            "wrong Host",
            request("127.0.0.1:1", Some(&origin), &cookie, Some(&csrf)),
        ),
        (
            "wrong Origin",
            request(&host, Some("http://127.0.0.1:1"), &cookie, Some(&csrf)),
        ),
        ("missing Origin", request(&host, None, &cookie, Some(&csrf))),
        (
            "unknown cookie",
            request(&host, Some(&origin), &unknown, Some(&csrf)),
        ),
        (
            "wrong CSRF",
            request(&host, Some(&origin), &cookie, Some(&unknown)),
        ),
        ("missing CSRF", request(&host, Some(&origin), &cookie, None)),
    ] {
        let (head, response) = raw_onboard(addr, &bytes);
        assert!(
            head.starts_with("HTTP/1.1 403 Forbidden"),
            "{label}: {head}"
        );
        assert!(response.is_empty(), "{label}: no envelope");
    }
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let (expired_cookie, expired_csrf) = exchange(addr, &issue(&settings).bootstrap);
    clock.advance(30 * 60 * 1000);
    let (fresh_cookie, fresh_csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, response) = post_answer(
        addr,
        &expired_cookie,
        &expired_csrf,
        &answer_body(&request_id(1), agent, question, "main"),
    );
    empty_refusal(&head, &response, "HTTP/1.1 403 Forbidden");
    assert_eq!(
        fixture.records(),
        before,
        "an expired session appends nothing"
    );
    let _ = (cookie, csrf);

    let (head, response) = post_answer(
        addr,
        &fresh_cookie,
        &fresh_csrf,
        &answer_body(&request_id(1), agent, question, "main"),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "control: {head}");
    assert_eq!(response["state"], "answered");
    assert_eq!(answers_for(&fixture.records(), agent), 1);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The pair `(agent, question_lsn)` must name that agent's open question. Another agent's
/// question, an unknown agent, and a question already answered locally — and re-asked since —
/// are each `question_closed`, with no effect. The local answer uses the one production-positive
/// local path: a connection holding its authority from before the first hire. Control: the
/// re-asked question answers.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "four refused pairs and the control share one daemon, one session and one local path"
)]
fn browser_answer_refuses_wrong_agent_stale_and_locally_answered_questions() {
    let fixture = Fixture::new();
    let settings = answer_settings(&fixture, true);
    let column = common::ScriptedColumn::new(dudes_proto::PeerProvenance::NoRecordedAgent);
    let daemon = boot(
        &settings,
        common::seams_with(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
            Arc::clone(&column) as Arc<dyn dudes_rpc::PeerResolver>,
        ),
    )
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let credential = || {
        OperatorCredential::new(
            std::fs::read_to_string(
                settings
                    .runtime_dir
                    .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
            )
            .expect("operator credential"),
        )
        .expect("credential")
    };
    let mut operator = Client::connect(settings.socket()).expect("connect before the first hire");
    let hired = operator
        .call_as_operator(
            Method::AgentHire,
            Some(serde_json::to_value(common::hire_params(fixture.state.path())).unwrap()),
            credential(),
        )
        .expect("the first hire, on the pre-hire connection");
    let a = match hired.result {
        Some(result) => {
            serde_json::from_value::<dudes_proto::rpc::agent::HireResult>(result)
                .expect("hire result")
                .agent
        }
        None => panic!("agent.hire: {hired:?}"),
    };
    running
        .handle
        .ask(Ask::Ask {
            agent: a,
            text: "which branch?".to_owned(),
        })
        .expect("the question");
    common::set_goal(&running.handle, common::goal_params(a, &[true_program()]));
    let first = question_lsn(&fixture, a);
    let (b, other) = waiting_agent(&running, &fixture, "which tag?");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);

    let local = operator
        .call_as_operator(
            Method::AgentAnswer,
            Some(json!({ "agent": a, "text": "main, locally" })),
            credential(),
        )
        .expect("local answer response");
    assert!(
        local.error.is_none(),
        "the pre-hire connection applies: {local:?}"
    );
    running
        .handle
        .ask(Ask::Ask {
            agent: a,
            text: "and the tag?".to_owned(),
        })
        .expect("the re-asked question");
    let reasked = question_lsn(&fixture, a);
    assert_ne!(reasked, first);

    await_continuation_outcome(&fixture, &running.handle, a);
    let before = fixture.records();
    for (label, body) in [
        (
            "another agent's question",
            answer_body(&request_id(1), b, first, "x"),
        ),
        (
            "an unknown agent",
            answer_body(&request_id(2), AgentId::from_parts(9, 9), first, "x"),
        ),
        (
            "a locally answered question",
            answer_body(&request_id(3), a, first, "x"),
        ),
        (
            "the other agent's question named for this one",
            answer_body(&request_id(4), a, other, "x"),
        ),
    ] {
        let (head, response) = post_answer(addr, &cookie, &csrf, &body);
        assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{label}: {head}");
        assert_eq!(
            response,
            json!({ "request_id": body["request_id"], "state": "refused", "reason": "question_closed" }),
            "{label}"
        );
    }
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let (head, response) = post_answer(
        addr,
        &cookie,
        &csrf,
        &answer_body(&request_id(5), a, reasked, "v1.2"),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "control: {head}");
    assert_eq!(response["question_lsn"], json!(reasked.0.to_string()));
    assert_eq!(
        answers_for(&fixture.records(), a),
        2,
        "one local, one browser"
    );
    assert_eq!(answers_for(&fixture.records(), b), 0);
    drop(operator);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

fn seed(fixture: &Fixture, records: impl IntoIterator<Item = Record>) {
    let options = dudes_journal::JournalOptions {
        fsync: dudes_journal::Fsync::Full,
        checkpoint_every: 0,
        ..dudes_journal::JournalOptions::default()
    };
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), options).expect("open the seed journal");
    for record in records {
        writer
            .append(&dudes_proto::Entry {
                at: dudes_types::Timestamp::from_unix_millis(1_787_394_800_000),
                record,
            })
            .expect("seed one record");
    }
}

/// The refusal reasons in order: a retired agent is `agent_retired` before anything else, an attached
/// one is `agent_attached`, and a paused agent with an open question is answerable — the control gate then
/// holds its next turn.
#[test]
fn browser_answer_refuses_attached_retired_and_answers_paused() {
    let fixture = Fixture::new();
    let settings = answer_settings(&fixture, true);
    let first = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot the setup run");
    let mut running = common::start(first);
    let (attached, attached_q) = waiting_agent(&running, &fixture, "attached?");
    let (retired, retired_q) = waiting_agent(&running, &fixture, "retired?");
    let (both, both_q) = waiting_agent(&running, &fixture, "retired and attached?");
    let (paused, paused_q) = waiting_agent(&running, &fixture, "paused?");
    for agent in [retired, both] {
        running
            .handle
            .ask(Ask::Retire(Box::new(RetireParams {
                agent,
                reason: "done".to_owned(),
            })))
            .expect("retire");
    }
    running.handle.ask(Ask::Pause(paused)).expect("pause");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    seed(
        &fixture,
        [attached, both].map(|agent| Record::Attached {
            agent,
            by: dudes_proto::Actor::System,
            hook_port: None,
            provenance: None,
        }),
    );

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
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    for (k, agent, question, reason) in [
        (1, attached, attached_q, "agent_attached"),
        (2, retired, retired_q, "agent_retired"),
        (3, both, both_q, "agent_retired"),
    ] {
        let (head, response) = post_answer(
            addr,
            &cookie,
            &csrf,
            &answer_body(&request_id(k), agent, question, "x"),
        );
        assert!(
            head.starts_with("HTTP/1.1 409 Conflict"),
            "{reason}: {head}"
        );
        assert_eq!(response["reason"], reason, "{response}");
    }
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let (head, response) = post_answer(
        addr,
        &cookie,
        &csrf,
        &answer_body(&request_id(4), paused, paused_q, "yes"),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(response["state"], "answered");
    let after: Vec<Record> = fixture.records()[before.len()..].to_vec();
    assert_eq!(answers_for(&after, paused), 1);
    assert!(after.iter().any(|record| matches!(
        record,
        Record::LoopDecided {
            agent: who,
            decision: LoopDecision::Hold { reason: dudes_proto::HoldReason::Paused },
            ..
        } if *who == paused
    )));
    assert!(
        !after
            .iter()
            .any(|record| matches!(record, Record::TurnIssued { .. })),
        "the control gate holds the paused agent's next turn"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The journal-derived index answers a same-key retry with the original result and no new
/// record, even from a fresh session after a restart; the same key with a changed payload is an
/// empty `409`.
#[test]
fn browser_answer_same_key_retry_returns_original_and_changed_payload_conflicts() {
    let fixture = Fixture::new();
    let settings = answer_settings(&fixture, true);
    let run = || {
        boot(
            &settings,
            seams(
                &settings,
                SharedHost::new(),
                Arc::new(ScriptedRunner::default()),
                Arc::new(TestClock::new()),
                Witness::default(),
            ),
        )
        .expect("boot Floor")
    };
    let daemon = run();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (agent, question) = waiting_agent(&running, &fixture, "which branch?");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = answer_body(&request_id(1), agent, question, "main");
    let (head, original) = post_answer(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    await_continuation_outcome(&fixture, &running.handle, agent);
    let durable = fixture.records();

    let (head, retry) = post_answer(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(retry, original);
    assert_eq!(fixture.records(), durable, "a retry is read-only");

    let changed = answer_body(&request_id(1), agent, question, "develop");
    let (head, response) = post_answer(addr, &cookie, &csrf, &changed);
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    assert_eq!(fixture.records(), durable);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let daemon = run();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    running.handle.doctor().unwrap();
    let settled = fixture.records();
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, replayed) = post_answer(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "after restart: {head}");
    assert_eq!(replayed, original);
    let (head, response) = post_answer(addr, &cookie, &csrf, &changed);
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    assert_eq!(
        fixture.records(),
        settled,
        "replayed truth answers; nothing is appended"
    );
    assert_eq!(answers_for(&fixture.records(), agent), 1);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// A same-key follower of an in-flight answer gets `202 pending` at once, and a changed payload
/// under that key an empty `409`. Two keys racing for one question are serialized on the writer:
/// one `HumanAnswer`, and the loser is `question_closed`.
#[test]
fn browser_answer_concurrent_same_key_is_pending_and_two_keys_yield_one_answer() {
    let fixture = Fixture::new();
    let settings = answer_settings(&fixture, true);
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
    .expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (first, first_q) = waiting_agent(&running, &fixture, "which branch?");
    let (second, second_q) = waiting_agent(&running, &fixture, "which tag?");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);

    pause.arm(OnboardingPausePoint::BeforeAnswerPermit);
    let owner_body = answer_body(&request_id(1), first, first_q, "main");
    let owner = {
        let (cookie, csrf, body) = (cookie.clone(), csrf.clone(), owner_body.clone());
        std::thread::spawn(move || post_answer(addr, &cookie, &csrf, &body))
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "owner is in flight"
    );
    // Both must answer while the owner still holds the writer. Each waits a bounded time; the
    // writer is released before any assertion, so a follower that queued behind it fails the
    // test rather than hanging it.
    let at_once = |body: Value| {
        let (tx, rx) = mpsc::channel();
        let (cookie, csrf) = (cookie.clone(), csrf.clone());
        std::thread::spawn(move || {
            let _ = tx.send(post_answer(addr, &cookie, &csrf, &body));
        });
        rx.recv_timeout(Duration::from_secs(3)).ok()
    };
    let follower = at_once(owner_body.clone());
    let changed = at_once(answer_body(&request_id(1), first, first_q, "develop"));
    pause.release();
    let (head, pending) =
        follower.expect("a same-key follower answers at once, without the writer");
    assert!(head.starts_with("HTTP/1.1 202 Accepted"), "{head}");
    assert_eq!(
        pending,
        json!({ "request_id": request_id(1), "state": "pending" })
    );
    let (head, response) = changed.expect("a changed payload answers at once, without the writer");
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    let (head, answered) = owner.join().expect("owner");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(answered["state"], "answered");

    pause.arm(OnboardingPausePoint::BeforeAnswerPermit);
    let winner = {
        let (cookie, csrf) = (cookie.clone(), csrf.clone());
        let body = answer_body(&request_id(2), second, second_q, "v1");
        std::thread::spawn(move || post_answer(addr, &cookie, &csrf, &body))
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "winner is in flight"
    );
    let (sent_tx, sent_rx) = mpsc::channel();
    let loser = {
        let (cookie, csrf) = (cookie.clone(), csrf.clone());
        let body =
            serde_json::to_vec(&answer_body(&request_id(3), second, second_q, "v2")).unwrap();
        std::thread::spawn(move || {
            post_bytes(addr, "/floor/answer", &cookie, &csrf, &body, Some(sent_tx))
        })
    };
    sent_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the second key is sent while the first holds the writer");
    std::thread::sleep(Duration::from_millis(100));
    pause.release();
    let (head, won) = winner.join().expect("winner");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(won["state"], "answered");
    let (head, lost) = loser.join().expect("loser");
    assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
    assert_eq!(lost["reason"], "question_closed");
    let records = fixture.records();
    assert_eq!(answers_for(&records, first), 1);
    assert_eq!(answers_for(&records, second), 1, "one question, one answer");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Across restarts, at three cuts: after the answer's fsync and before its fold; after its
/// fold and before its decision; after the continuation's `TurnIssued` fsync and before Spawn.
/// Each replays one answer and one continuation, exactly.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "three restart cuts share one assertion shape and stay side by side"
)]
fn browser_answer_restart_cut_points_replay_one_answer_and_one_continuation() {
    #[derive(Clone, Copy, Debug)]
    enum Cut {
        AnswerFsync,
        AnswerFold,
        TurnIssuedFsync,
    }
    for cut in [Cut::AnswerFsync, Cut::AnswerFold, Cut::TurnIssuedFsync] {
        let fixture = Fixture::new();
        let settings = answer_settings(&fixture, true);
        let host = SharedHost::new();
        let run = |host: &SharedHost| {
            boot(
                &settings,
                seams(
                    &settings,
                    host.clone(),
                    Arc::new(ScriptedRunner::default()),
                    Arc::new(TestClock::new()),
                    Witness::default(),
                ),
            )
            .expect("boot Floor")
        };
        let daemon = run(&host);
        let failure = daemon.onboarding_failure();
        let append_failure = daemon.append_failure();
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (agent, question) = waiting_agent(&running, &fixture, "which branch?");
        let (other, other_q) = waiting_agent(&running, &fixture, "which tag?");
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let body = answer_body(&request_id(1), agent, question, "main");
        match cut {
            Cut::AnswerFsync => failure.arm(OnboardingFailurePoint::Answer),
            // The answer is the first append of the request and its decision the second.
            Cut::AnswerFold => append_failure.fail_nth(NonZeroUsize::new(2).unwrap()),
            Cut::TurnIssuedFsync => failure.arm(OnboardingFailurePoint::TurnIssued),
        }
        let (head, response) = post_answer(addr, &cookie, &csrf, &body);
        match cut {
            Cut::AnswerFsync => {
                empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
                let (head, pending) = post_answer(addr, &cookie, &csrf, &body);
                assert!(head.starts_with("HTTP/1.1 202 Accepted"), "{head}");
                assert_eq!(
                    pending["state"], "pending",
                    "an uncertain append keeps its key"
                );
                let fresh = answer_body(&request_id(2), other, other_q, "v1");
                let (head, response) = post_answer(addr, &cookie, &csrf, &fresh);
                empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
            }
            Cut::AnswerFold | Cut::TurnIssuedFsync => {
                assert!(head.starts_with("HTTP/1.1 200 OK"), "{cut:?}: {head}");
                assert_eq!(response["state"], "answered", "durable and indexed");
            }
        }
        let spawned_before = host.spawned().len();
        assert_eq!(spawned_before, 0, "{cut:?}: the cut is before any Spawn");
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

        let daemon = run(&host);
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        fixture.await_record("the continuation's spawn", |record| {
            matches!(record, Record::TurnSpawned { agent: who, .. } if *who == agent).then_some(())
        });
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let (head, replayed) = post_answer(addr, &cookie, &csrf, &body);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{cut:?}: {head}");
        assert_eq!(replayed["question_lsn"], json!(question.0.to_string()));
        let records = fixture.records();
        let answered_lsn = fixture
            .entries()
            .into_iter()
            .find_map(|(lsn, e)| matches!(e.record, Record::HumanAnswer { .. }).then_some(lsn))
            .expect("answer");
        assert_eq!(answers_for(&records, agent), 1, "{cut:?}: one answer");
        let issued: Vec<&Vec<Lsn>> = records
            .iter()
            .filter_map(|record| match record {
                Record::TurnIssued {
                    agent: who,
                    answers,
                    ..
                } if *who == agent => Some(answers),
                _ => None,
            })
            .collect();
        assert_eq!(
            issued,
            vec![&vec![answered_lsn]],
            "{cut:?}: one continuation carrying it"
        );
        assert_eq!(host.spawned().len(), 1, "{cut:?}: one Spawn");
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, Record::TurnSpawned { agent: who, .. } if *who == agent))
                .count(),
            1,
            "{cut:?}"
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

/// A clock that tells the test when a Floor connection thread reads it: the first such read after
/// [`ProbeClock::arm`] signals, after the value is taken.
struct ProbeClock {
    inner: TestClock,
    armed: Mutex<Option<mpsc::Sender<()>>>,
}

impl ProbeClock {
    fn arm(&self) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        *self.armed.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
        rx
    }
}

impl dudes_supervisor::Clock for ProbeClock {
    fn now(&self) -> dudes_types::Timestamp {
        let now = dudes_supervisor::Clock::now(&self.inner);
        // Admission samples session validity on the request's own connection thread. The daemon
        // loop, the day watch and the test thread read the same clock on their own schedule and
        // hold no session lock, so a read there says nothing about when admission sampled.
        if std::thread::current().name() != Some("dudes-floor-connection") {
            return now;
        }
        if let Some(signal) = self
            .armed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = signal.send(());
        }
        now
    }
}

/// Session validity is sampled only after the registry lock is taken.
/// Session A holds its answer's append permit — and so the registry lock — while session B's
/// answer waits for admission and the test clock crosses the absolute expiry. A's held permit wins
/// through its fsync; B gets the uniform empty `403` and appends nothing; and no clock read
/// happens while A holds the lock.
///
/// The outcome alone cannot catch a read taken before the lock at admission, because the
/// reservation re-prunes under the lock afterwards; the probe's ordering assertion is what does.
#[test]
fn browser_answer_expiry_waiting_on_registry_lock_is_forbidden() {
    let fixture = Fixture::new();
    let settings = answer_settings(&fixture, true);
    let clock = Arc::new(ProbeClock {
        inner: TestClock::ticking(1_800_000_000_000, 0),
        armed: Mutex::new(None),
    });
    let daemon = boot(
        &settings,
        Seams {
            host: SharedHost::new(),
            pty_host: Box::new(duded::ports::ClosedPtyHost),
            predicates: Arc::new(ScriptedRunner::default()),
            clock: Arc::clone(&clock),
            rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
            resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
            adapters: [Box::new(StubAdapter) as Box<dyn Adapter>]
                .into_iter()
                .collect(),
            children: ChildEnds::new(),
            witness: Witness::default(),
        },
    )
    .expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (a, a_q) = waiting_agent(&running, &fixture, "which branch?");
    let (b, b_q) = waiting_agent(&running, &fixture, "which tag?");
    let (a_cookie, a_csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (b_cookie, b_csrf) = exchange(addr, &issue(&settings).bootstrap);

    pause.arm(OnboardingPausePoint::AfterAnswerPermit);
    let holder = std::thread::spawn(move || {
        post_answer(
            addr,
            &a_cookie,
            &a_csrf,
            &answer_body(&request_id(1), a, a_q, "main"),
        )
    });
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "A holds its append permit"
    );
    let probe = clock.arm();
    let (sent_tx, sent_rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let body = serde_json::to_vec(&answer_body(&request_id(2), b, b_q, "v1")).unwrap();
        post_bytes(
            addr,
            "/floor/answer",
            &b_cookie,
            &b_csrf,
            &body,
            Some(sent_tx),
        )
    });
    sent_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("B is sent while A holds the lock");
    let sampled_while_held = probe.recv_timeout(Duration::from_millis(500)).is_ok();
    clock.inner.advance(8 * 60 * 60 * 1000);
    pause.release();
    let (head, winner) = holder.join().expect("A");
    assert!(
        head.starts_with("HTTP/1.1 200 OK"),
        "A's held permit wins: {head}"
    );
    assert_eq!(winner["state"], "answered");
    let (head, response) = waiter.join().expect("B");
    assert!(
        probe.recv_timeout(Duration::from_secs(2)).is_ok() || sampled_while_held,
        "positive control: the clock was read once armed"
    );
    assert!(
        !sampled_while_held,
        "session validity was sampled while another session held the registry lock"
    );
    empty_refusal(&head, &response, "HTTP/1.1 403 Forbidden");
    let records = fixture.records();
    assert_eq!(answers_for(&records, a), 1);
    assert_eq!(
        answers_for(&records, b),
        0,
        "the expired waiter appended nothing"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// A correctly identified live session passes the world check, is revoked before its append
/// permit, and gets the uniform empty `403` with no `HumanAnswer`, `LoopDecided` or
/// `TurnIssued`. Control: the same answer from a fresh session applies.
///
/// The two mutants turn this red: the permit match omitting an attributed
/// `HumanAnswer`, and `permit` returning without consulting the registry. Deleting only the
/// `revoked` flag check is not a mutant: revocation also removes the session from the registry.
#[test]
fn browser_answer_refuses_revoked_session_without_effect() {
    let fixture = Fixture::new();
    let settings = answer_settings(&fixture, true);
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
    .expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let authorizer = daemon.floor_session_authorizer().expect("Floor sessions");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (agent, question) = waiting_agent(&running, &fixture, "which branch?");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();

    pause.arm(OnboardingPausePoint::BeforeAnswerPermit);
    let revoked = {
        let (cookie, csrf) = (cookie.clone(), csrf.clone());
        std::thread::spawn(move || {
            post_answer(
                addr,
                &cookie,
                &csrf,
                &answer_body(&request_id(1), agent, question, "main"),
            )
        })
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "admitted, world-checked, before the permit"
    );
    assert!(
        authorizer.revoke_for_test(&cookie),
        "the live session is revoked"
    );
    pause.release();
    let (head, response) = revoked.join().expect("revoked request");
    empty_refusal(&head, &response, "HTTP/1.1 403 Forbidden");
    let after = fixture.records();
    assert!(
        !after[before.len()..].iter().any(|record| matches!(
            record,
            Record::HumanAnswer { .. } | Record::LoopDecided { .. } | Record::TurnIssued { .. }
        )),
        "{:?}",
        &after[before.len()..]
    );

    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, response) = post_answer(
        addr,
        &cookie,
        &csrf,
        &answer_body(&request_id(1), agent, question, "main"),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "control: {head}");
    assert_eq!(response["state"], "answered");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// An uncertain local write fences the daemon: a durable answer's same-key retry is still served
/// from the index, with its original result and a changed payload's conflict, while a fresh
/// request gets an empty `503`, appends nothing, and releases its key.
#[test]
fn browser_answer_uncertain_local_write_fences_fresh_requests_and_serves_durable_retries() {
    let fixture = Fixture::new();
    let settings = answer_settings(&fixture, true);
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
    .expect("boot Floor");
    let failure = daemon.onboarding_failure();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (first, first_q) = waiting_agent(&running, &fixture, "which branch?");
    let (second, second_q) = waiting_agent(&running, &fixture, "which tag?");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let durable = answer_body(&request_id(1), first, first_q, "main");
    let (head, original) = post_answer(addr, &cookie, &csrf, &durable);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");

    failure.arm(OnboardingFailurePoint::LocalGoal);
    let local = running
        .handle
        .ask(Ask::SetGoal(Box::new(common::goal_params(
            first,
            &[true_program()],
        ))));
    assert!(
        local.is_err(),
        "the local goal is durable and then fails its fold"
    );
    let fenced = fixture.records();

    for attempt in 0..2 {
        let (head, retry) = post_answer(addr, &cookie, &csrf, &durable);
        assert!(
            head.starts_with("HTTP/1.1 200 OK"),
            "retry {attempt} while fenced: {head}"
        );
        assert_eq!(retry, original);
    }
    let (head, response) = post_answer(
        addr,
        &cookie,
        &csrf,
        &answer_body(&request_id(1), first, first_q, "develop"),
    );
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    let fresh = answer_body(&request_id(2), second, second_q, "v1");
    for attempt in 0..2 {
        let (head, response) = post_answer(addr, &cookie, &csrf, &fresh);
        empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
        let _ = attempt;
    }
    assert_eq!(fixture.records(), fenced, "nothing appended while fenced");
    assert_eq!(answers_for(&fixture.records(), second), 0);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The request bounds refuse before any effect: 4,097 bytes, empty after trimming, U+0000, an interior
/// CR, a lone surrogate, an unknown, a duplicate and a nested member, a noncanonical request
/// identity or question position, the four valid values as a positional array, and a 16,385-byte
/// body. Controls: 4,096 bytes and a valid 16,384-byte body answer.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "every request bound is one row of a single table beside its two controls"
)]
fn browser_answer_bounds_and_json_shape_refuse_before_effect() {
    let fixture = Fixture::new();
    let settings = answer_settings(&fixture, true);
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
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (agent, question) = waiting_agent(&running, &fixture, "which branch?");
    let (second, second_q) = waiting_agent(&running, &fixture, "which tag?");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let id = request_id(1);
    let q = question.0.to_string();
    let valid_prefix =
        format!(r#"{{"request_id":"{id}","agent":"{agent}","question_lsn":"{q}","text":"#);
    // The same id under Crockford's alias `O` for `0`: one value, a second, noncanonical name.
    let alias = agent.to_string().replacen('0', "O", 1);
    assert_ne!(alias, agent.to_string(), "the alias spelling differs");
    let refusals: Vec<(&str, Vec<u8>)> = vec![
        (
            "4,097 bytes",
            format!(r#"{valid_prefix}"{}"}}"#, "a".repeat(4_097)).into_bytes(),
        ),
        (
            "empty after trimming",
            format!(r#"{valid_prefix}" \t\r\n "}}"#).into_bytes(),
        ),
        (
            "U+0000",
            format!(r#"{valid_prefix}"a\u0000b"}}"#).into_bytes(),
        ),
        (
            "interior CR",
            format!(r#"{valid_prefix}"a\rb"}}"#).into_bytes(),
        ),
        ("DEL", format!(r#"{valid_prefix}"a\u007fb"}}"#).into_bytes()),
        (
            "lone surrogate",
            format!(r#"{valid_prefix}"a\ud800b"}}"#).into_bytes(),
        ),
        (
            "unknown member",
            format!(r#"{valid_prefix}"main","extra":"x"}}"#).into_bytes(),
        ),
        (
            "duplicate member",
            format!(r#"{valid_prefix}"main","text":"again"}}"#).into_bytes(),
        ),
        (
            "nested member",
            format!(r#"{valid_prefix}{{"nested":"main"}}}}"#).into_bytes(),
        ),
        (
            "noncanonical request_id",
            format!(
                r#"{{"request_id":"{}B","agent":"{agent}","question_lsn":"{q}","text":"main"}}"#,
                "A".repeat(42)
            )
            .into_bytes(),
        ),
        (
            "short request_id",
            format!(
                r#"{{"request_id":"{}","agent":"{agent}","question_lsn":"{q}","text":"main"}}"#,
                "A".repeat(42)
            )
            .into_bytes(),
        ),
        (
            "leading-zero question_lsn",
            format!(
                r#"{{"request_id":"{id}","agent":"{agent}","question_lsn":"0{q}","text":"main"}}"#
            )
            .into_bytes(),
        ),
        (
            "numeric question_lsn",
            format!(
                r#"{{"request_id":"{id}","agent":"{agent}","question_lsn":{q},"text":"main"}}"#
            )
            .into_bytes(),
        ),
        (
            "alias-letter agent",
            format!(
                r#"{{"request_id":"{id}","agent":"{alias}","question_lsn":"{q}","text":"main"}}"#
            )
            .into_bytes(),
        ),
        (
            "an array",
            format!(r#"["{id}","{agent}","{q}","main"]"#).into_bytes(),
        ),
    ];
    let before = fixture.records();
    for (label, body) in &refusals {
        let (head, response) = post_bytes(addr, "/floor/answer", &cookie, &csrf, body, None);
        assert!(
            head.starts_with("HTTP/1.1 400 Bad Request"),
            "{label}: {head}"
        );
        assert!(response.is_null(), "{label}: {response}");
    }
    let host = addr.to_string();
    let oversize = format!(
        "POST /floor/answer HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: dudes_floor_session={cookie}\r\nX-Dudes-CSRF: {csrf}\r\nContent-Type: application/json\r\nContent-Length: 16385\r\n\r\n"
    );
    let (head, body) = raw_onboard(addr, oversize.as_bytes());
    assert!(
        head.starts_with("HTTP/1.1 413"),
        "refused before a byte is read: {head}"
    );
    assert!(body.is_empty());
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let whole = "a".repeat(4_096);
    let (head, response) = post_answer(
        addr,
        &cookie,
        &csrf,
        &answer_body(&request_id(2), agent, question, &whole),
    );
    assert!(
        head.starts_with("HTTP/1.1 200 OK"),
        "control, 4,096 bytes: {head}"
    );
    assert_eq!(response["state"], "answered");

    // A valid request padded with insignificant whitespace to exactly the cap.
    let compact =
        serde_json::to_string(&answer_body(&request_id(3), second, second_q, "v1")).unwrap();
    let padded = format!("{}{}", compact, " ".repeat(16_384 - compact.len()));
    assert_eq!(padded.len(), 16_384);
    let (head, response) = post_bytes(
        addr,
        "/floor/answer",
        &cookie,
        &csrf,
        padded.as_bytes(),
        None,
    );
    assert!(
        head.starts_with("HTTP/1.1 200 OK"),
        "control, a 16,384-byte body: {head}"
    );
    assert_eq!(response["state"], "answered");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// With answering off — the default — `POST /floor/answer` is not a route: its response is byte
/// for byte an unrouted POST's, and no snapshot carries the `answer` sibling. Control: the same
/// daemon configuration with answering on routes it and sends the sibling.
#[test]
fn browser_answer_is_absent_when_not_enabled() {
    for enabled in [false, true] {
        let fixture = Fixture::new();
        let settings = answer_settings(&fixture, enabled);
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
        .expect("boot Floor");
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (agent, question) = waiting_agent(&running, &fixture, "which branch?");
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let body =
            serde_json::to_vec(&answer_body(&request_id(1), agent, question, "main")).unwrap();
        let host = addr.to_string();
        let raw = |path: &str| {
            let mut stream = TcpStream::connect(addr).expect("Floor listener");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            let mut request = format!(
                "POST {path} HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: dudes_floor_session={cookie}\r\nX-Dudes-CSRF: {csrf}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .into_bytes();
            request.extend_from_slice(&body);
            stream.write_all(&request).expect("request");
            let mut response = Vec::new();
            let _ = stream.read_to_end(&mut response);
            response
        };
        let (snapshot, _) = snapshot(addr, &cookie, &csrf);
        let answered = raw("/floor/answer");
        let unrouted = raw("/floor/unrouted");
        if enabled {
            assert_ne!(answered, unrouted, "control: the enabled route is a route");
            assert!(String::from_utf8_lossy(&answered).starts_with("HTTP/1.1 200 OK"));
            assert_eq!(snapshot["answer"], json!({ "max_text_bytes": 4096 }));
        } else {
            assert!(!unrouted.is_empty());
            assert_eq!(
                answered,
                unrouted,
                "disabled is exactly unrouted: {}",
                String::from_utf8_lossy(&answered)
            );
            assert!(snapshot.get("answer").is_none(), "{snapshot}");
            assert_eq!(answers_for(&fixture.records(), agent), 0);
        }
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

/// `[floor.answer] enabled` is the operator's switch: absent means off, `true` turns it on, and
/// an unknown key in the table is refused rather than ignored.
#[test]
fn operator_answer_switch_is_off_unless_enabled() {
    let root = tempfile::tempdir().expect("operator config root");
    let config = root.path().join("config.toml");
    std::fs::write(&config, "[budget]\nfloor_usd_per_day = 10\n").expect("no answer table");
    assert!(!load_operator(&config).expect("valid").answer_enabled);
    std::fs::write(&config, "[floor.answer]\n").expect("empty answer table");
    assert!(!load_operator(&config).expect("valid").answer_enabled);
    std::fs::write(&config, "[floor.answer]\nenabled = true\n").expect("enabled");
    assert!(load_operator(&config).expect("valid").answer_enabled);
    std::fs::write(&config, "[floor.answer]\nenabled = true\nlimit = 3\n").expect("unknown key");
    assert!(load_operator(&config).is_err(), "an unknown key is refused");
    std::fs::write(&config, "[floor.answer]\nenabled = \"yes\"\n").expect("wrong type");
    assert!(
        load_operator(&config).is_err(),
        "a non-boolean switch is refused"
    );
}

// --- Pause and resume one agent ---------------------------------------------------

const PAUSE: &str = "/floor/pause";
const RESUME: &str = "/floor/resume";

/// Floor settings with pause and resume on or off. Nothing else differs from the fixture's.
fn control_settings(fixture: &Fixture, enabled: bool) -> duded::Settings {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.floor_control = if enabled {
        FloorControl::On {
            goal: false,
            retire: false,
            renew: false,
        }
    } else {
        FloorControl::Off
    };
    settings
}

fn control_body(request_id: &str, agent: AgentId, control_lsn: Option<Lsn>) -> Value {
    json!({
        "request_id": request_id,
        "agent": agent,
        "control_lsn": control_lsn.map(|lsn| lsn.0.to_string()),
    })
}

fn post_control(
    addr: SocketAddr,
    path: &str,
    cookie: &str,
    csrf: &str,
    body: &Value,
) -> (String, Value) {
    post_bytes(
        addr,
        path,
        cookie,
        csrf,
        &serde_json::to_vec(body).expect("control JSON"),
        None,
    )
}

/// One complete raw exchange, for byte-for-byte comparison of two responses.
fn raw_post(addr: SocketAddr, path: &str, cookie: &str, csrf: &str, body: &[u8]) -> Vec<u8> {
    let host = addr.to_string();
    let mut stream = TcpStream::connect(addr).expect("Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: dudes_floor_session={cookie}\r\nX-Dudes-CSRF: {csrf}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body);
    stream.write_all(&request).expect("request");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    response
}

/// The agent's control identity on disk: the position of its newest `Pause`, `Resume` or `Halt`.
fn disk_control_lsn(fixture: &Fixture, agent: AgentId) -> Option<Lsn> {
    fixture
        .entries()
        .into_iter()
        .rev()
        .find_map(|(lsn, entry)| match entry.record {
            Record::Pause { agent: who, .. }
            | Record::Resume { agent: who, .. }
            | Record::Halt { agent: who, .. }
                if who == agent =>
            {
                Some(lsn)
            }
            _ => None,
        })
}

/// How many `Pause` and `Resume` records name `agent`.
fn controls_for(records: &[Record], agent: AgentId) -> (usize, usize) {
    let pauses = records
        .iter()
        .filter(|record| matches!(record, Record::Pause { agent: who, .. } if *who == agent))
        .count();
    let resumes = records
        .iter()
        .filter(|record| matches!(record, Record::Resume { agent: who, .. } if *who == agent))
        .count();
    (pauses, resumes)
}

/// A `200` naming the route's state; returns the control identity the record set.
fn applied(head: &str, response: &Value, state: &str) -> Lsn {
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {response}");
    assert!(head.contains("Cache-Control: no-store"), "{head}");
    assert_eq!(response["state"], state, "{response}");
    Lsn(response["control_lsn"]
        .as_str()
        .expect("a decimal string")
        .parse()
        .expect("a u64"))
}

/// A `409` refusal with this reason and no effect of its own.
fn refused(head: &str, response: &Value, reason: &str) {
    assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
    assert_eq!(response["state"], "refused", "{response}");
    assert_eq!(response["reason"], reason, "{response}");
}

/// The request digest, recomputed here rather than taken from the daemon.
fn expected_control_digest(path: &str, agent: AgentId, control_lsn: Option<Lsn>) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(if path == PAUSE {
        b"dudes.pause.v1\0".as_slice()
    } else {
        b"dudes.resume.v1\0".as_slice()
    });
    for (field, value) in [
        ("agent", agent.to_string()),
        (
            "control_lsn",
            control_lsn.map(|lsn| lsn.0.to_string()).unwrap_or_default(),
        ),
    ] {
        hasher.update(u16::try_from(field.len()).unwrap().to_be_bytes());
        hasher.update(field.as_bytes());
        hasher.update(u32::try_from(value.len()).unwrap().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.finalize().into()
}

fn fake_seams(
    settings: &duded::Settings,
) -> Seams<SharedHost, ScriptedRunner, TestClock, common::FixtureRng> {
    seams(
        settings,
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    )
}

fn native_fixture_seams<P: duded::predicate::PredicateRunner, C: dudes_supervisor::Clock>(
    fixture: &Fixture,
    mut parts: Seams<SharedHost, P, C, common::FixtureRng>,
) -> (Seams<SharedHost, P, C, common::FixtureRng>, SharedHost) {
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    host.bind_child_ends(Arc::clone(&parts.children));
    parts.host = host.clone();
    (parts, host)
}

struct NativeTurnFixture<'a> {
    running: &'a common::Running,
    fixture: &'a Fixture,
    host: &'a SharedHost,
}

impl NativeTurnFixture<'_> {
    fn complete(&self, turn: TurnId, usage: Option<Usage>) {
        common::end_turn_with_native_payload(
            self.fixture,
            self.host,
            Input::ChildExited {
                final_reply: None,
                elapsed_ms: None,
                turn,
                exit: ExitStatus {
                    code: Some(0),
                    signal: None,
                },
                usage: usage.map(ScopedUsage::Turn),
                model_usage: std::collections::BTreeMap::new(),
                outcome: TurnOutcome::Completed,
                last_message: None,
            },
        );
    }
}

fn issued_goal_turn(fixture: &Fixture, goal: GoalId) -> TurnId {
    let turn = nth_turn(fixture, goal, 0);
    fixture.await_record("the goal's exact native spawn", |record| {
        matches!(record, Record::TurnSpawned { turn: seen, .. } if *seen == turn).then_some(())
    });
    turn
}

fn recover_owed_browser_turn(
    settings: &duded::Settings,
    fixture: &Fixture,
    running: &common::Running,
    agent: AgentId,
    goal: GoalId,
) {
    use dudes_proto::rpc::recovery::{
        RecoveryAllowedChoices, RecoveryChoice, RecoveryInspectResult, RecoverySubmitParams,
        RecoverySubmitResult,
    };

    let turn = nth_turn(fixture, goal, 0);
    fixture.await_record("the owed goal's recovery barrier", |record| {
        matches!(record, Record::RecoveryRequired(required)
            if required.binding.work.agent == agent && required.binding.work.goal == goal
                && required.binding.work.turn == turn)
        .then_some(())
    });
    let owed: Vec<_> = fixture
        .records()
        .into_iter()
        .filter(
            |record| matches!(record, Record::TurnIssued { goal: issued, .. } if *issued == goal),
        )
        .collect();
    assert_eq!(owed.len(), 1);
    let no_spawn = || {
        assert!(!fixture.records().iter().any(|record| {
            matches!(record, Record::TurnSpawned { turn: seen, .. } if *seen == turn)
        }));
    };
    no_spawn();
    let call = |method, params| {
        let credential = std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .expect("operator credential");
        let response = Client::connect(settings.socket())
            .expect("operator socket")
            .call_current_as_operator(
                method,
                Some(params),
                OperatorCredential::new(credential).expect("credential"),
            )
            .expect("operator response");
        assert!(response.error.is_none(), "{response:?}");
        response.result.expect("operator result")
    };
    call(
        CurrentMethod::Legacy(Method::LoopResume),
        json!({"agent":agent}),
    );
    running.handle.doctor().expect("Resume reached the writer");
    no_spawn();
    let inspection: RecoveryInspectResult =
        serde_json::from_value(call(CurrentMethod::RecoveryInspect, json!({"agent":agent})))
            .expect("typed recovery inspection");
    let RecoveryInspectResult::Available {
        expected,
        choices,
        reason,
        ..
    } = inspection
    else {
        panic!("the owed issue must offer exact recovery");
    };
    assert_eq!(choices, RecoveryAllowedChoices::RepeatIssuedFresh);
    assert_eq!(reason, dudes_proto::recovery::RecoveryReason::MissingSpawn);
    assert_eq!(expected.work.agent, agent);
    assert_eq!(expected.work.goal, goal);
    assert_eq!(expected.work.turn, turn);
    let request_id = Hex32::new([231; 32]);
    let request = RecoverySubmitParams {
        request_id,
        expected: *expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
    };
    let submitted: RecoverySubmitResult = serde_json::from_value(call(
        CurrentMethod::RecoverySubmit,
        serde_json::to_value(request).expect("strict recovery request"),
    ))
    .expect("typed recovery submission");
    let RecoverySubmitResult::Accepted { receipt } = submitted else {
        panic!("one explicit submission must authorize the exact repeat");
    };
    assert_eq!(receipt.request_id, request_id);
    assert_eq!(receipt.claim.episode.work, expected.work);
    let records = fixture.records();
    assert_eq!(records.iter().filter(|record| {
        matches!(record, Record::RecoveryAuthorized(authorized) if authorized.request_id == request_id)
    }).count(), 1);
    assert_eq!(
        records
            .into_iter()
            .filter(|record| {
                matches!(record, Record::TurnIssued { goal: issued, .. } if *issued == goal)
            })
            .collect::<Vec<_>>(),
        owed
    );
}

fn await_continuation_outcome(fixture: &Fixture, handle: &duded::Handle, agent: AgentId) -> TurnId {
    let turn = fixture.await_record(
        "the prior continuation's actual start",
        |record| match record {
            Record::InstructionObservationStarted {
                owner:
                    dudes_proto::InstructionObservationOwner::Managed {
                        agent: owner, turn, ..
                    },
                ..
            } if *owner == agent => Some(*turn),
            _ => None,
        },
    );
    fixture.await_record(
        "the prior continuation's actual outcome",
        |record| match record {
            Record::TurnSpawned { turn: seen, .. } if *seen == turn => Some(()),
            Record::InstructionObservationFinished {
                owner: dudes_proto::InstructionObservationOwner::Managed { turn: seen, .. },
                ..
            } if *seen == turn => Some(()),
            _ => None,
        },
    );
    handle.doctor().unwrap();
    turn
}

/// Every record after `lsn`.
fn after(fixture: &Fixture, lsn: Lsn) -> Vec<(Lsn, dudes_proto::Entry)> {
    fixture
        .entries()
        .into_iter()
        .filter(|(at, _)| *at > lsn)
        .collect()
}

/// The primary test, stub-backed and read from the replayed journal, in its eight
/// ordered steps: a pause lands while a Managed turn runs and kills nothing; the turn's end is
/// held by the control gate; no turn is issued until a resume presenting the pause's position; the resume
/// is admitted by the budget gate and issues exactly one more turn; and after replay the agent is unpaused
/// and the index answers both keys.
///
/// Mutants that must turn this red: the pause persists with the kill continuation; the
/// resume persists with a bare fold; the pause fold leaves the control identity unset; boot
/// replays no control index.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end stub scenario asserts its eight ordered steps on one journal"
)]
fn browser_pause_resume_is_finite_and_observed() {
    let fixture = Fixture::new();
    let capture = tempfile::tempdir().expect("task capture directory");
    let hold = capture.path().join("release-first-turn");
    let mut settings = control_settings(&fixture, true);
    settings.daemon_env.push(EnvVar::new(HELPER_ROLE, "1"));
    settings
        .daemon_env
        .push(EnvVar::new(TASK_CAPTURE, capture.path().as_os_str()));
    settings
        .daemon_env
        .push(EnvVar::new(STUB_HOLD, hold.as_os_str()));
    let spawns = || {
        std::fs::read_dir(capture.path())
            .expect("capture directory")
            .filter(|file| {
                file.as_ref()
                    .is_ok_and(|file| file.path().extension().is_some_and(|ext| ext == "task"))
            })
            .count()
    };
    let daemon = boot(
        &settings,
        real_stub_seams_with(
            &settings,
            ScriptedRunner::scripted([common::fails(), common::passes()]),
        ),
    )
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[true_program()]),
    );
    let t1 = fixture.await_record("T1 spawned", |record| match record {
        Record::TurnSpawned {
            agent: who, turn, ..
        } if *who == agent => Some(*turn),
        _ => None,
    });
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (view, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(view["control"], json!({}), "control is enabled");
    let row = view["projection"]["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .find(|row| row["id"] == json!(agent))
        .expect("the agent")
        .clone();
    assert!(
        row.get("control_lsn").is_none(),
        "no control record yet: {row}"
    );

    // Step 1: T1 is open; a pause presenting no control identity applies.
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::TurnEnded { turn, .. } if *turn == t1)),
        "T1 is still running"
    );
    let pause_wire = request_id(1);
    let (head, response) = post_control(
        addr,
        PAUSE,
        &cookie,
        &csrf,
        &control_body(&pause_wire, agent, None),
    );
    let p = applied(&head, &response, "paused");
    assert_eq!(
        response,
        json!({
            "request_id": pause_wire, "state": "paused", "agent": agent,
            "control_lsn": p.0.to_string(),
        })
    );

    // Step 2: the pause is at P and kills nothing.
    assert!(
        matches!(
            fixture
                .entries()
                .into_iter()
                .find(|(at, _)| *at == p)
                .map(|(_, e)| e.record),
            Some(Record::Pause {
                browser: Some(_),
                ..
            })
        ),
        "an attributed Pause at P"
    );
    assert!(
        !after(&fixture, p).iter().any(|(_, e)| matches!(
            &e.record,
            Record::TurnEnded { turn, .. } | Record::KillDeferred { turn, .. } if *turn == t1
        )),
        "the pause neither ended nor killed T1"
    );
    std::fs::write(&hold, b"").expect("release T1");

    // Step 3: T1's end is decided `Hold{Paused}`, with the control gate fired.
    let hold_trace = fixture.await_record("the held decision", |record| match record {
        Record::LoopDecided {
            agent: who,
            decision:
                LoopDecision::Hold {
                    reason: dudes_proto::HoldReason::Paused,
                },
            gate_trace,
            ..
        } if *who == agent => Some(gate_trace.clone()),
        _ => None,
    });
    assert!(
        hold_trace
            .iter()
            .any(|step| step.gate == dudes_proto::Gate::Control && step.fired),
        "{hold_trace:?}"
    );
    // A job through the same writer, so every effect of the held pump has been run.
    let _ = snapshot(addr, &cookie, &csrf);

    // Step 4: nothing is issued after P, and the stub saw one launch.
    let issued_after = |from: Lsn| {
        fixture
            .records()
            .iter()
            .filter(|record| {
                matches!(record, Record::TurnIssued { agent: who, lsn_of_decision, .. }
                    if *who == agent && *lsn_of_decision > from)
            })
            .count()
    };
    assert_eq!(issued_after(p), 0, "no TurnIssued decided after P");
    assert_eq!(spawns(), 1, "no new stub spawn while paused");
    let t1_end = fixture
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::TurnEnded { turn, outcome, .. } if turn == t1 => Some(outcome),
            _ => None,
        })
        .expect("T1 ended");
    assert!(
        !matches!(t1_end, Some(dudes_proto::TurnOutcome::Killed { .. })),
        "T1 ran to its own end: {t1_end:?}"
    );

    // Step 5: a resume presenting P applies at R.
    let resume_wire = request_id(2);
    let (head, response) = post_control(
        addr,
        RESUME,
        &cookie,
        &csrf,
        &control_body(&resume_wire, agent, Some(p)),
    );
    let r = applied(&head, &response, "resumed");
    assert!(r > p);
    fixture.await_record("the goal met after the resumed turn", |record| {
        matches!(
            record,
            Record::LoopDecided { agent: who, decision: LoopDecision::Done, .. } if *who == agent
        )
        .then_some(())
    });
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let entries = fixture.entries();
    let at = |lsn: Lsn| {
        entries
            .iter()
            .find(|(at, _)| *at == lsn)
            .map(|(_, e)| e.record.clone())
    };
    let Some(Record::Pause {
        agent: paused,
        browser: Some(pause_attribution),
    }) = at(p)
    else {
        panic!("an attributed Pause at P");
    };
    let Some(Record::Resume {
        agent: resumed,
        browser: Some(resume_attribution),
    }) = at(r)
    else {
        panic!("an attributed Resume at R");
    };
    assert_eq!((paused, resumed), (agent, agent));
    let session = <[u8; 32]>::from(<sha2::Sha256 as sha2::Digest>::digest(cookie.as_bytes()));
    let mut first_id = [0_u8; 32];
    first_id[0] = 4;
    assert_eq!(pause_attribution.request_id.bytes(), first_id);
    assert_eq!(pause_attribution.control_lsn, None);
    assert_eq!(
        pause_attribution.request_digest.bytes(),
        expected_control_digest(PAUSE, agent, None)
    );
    assert_eq!(pause_attribution.session_digest.bytes(), session);
    assert_eq!(resume_attribution.control_lsn, Some(p));
    assert_eq!(
        resume_attribution.request_digest.bytes(),
        expected_control_digest(RESUME, agent, Some(p))
    );
    assert!(
        entries.iter().any(|(lsn, e)| *lsn > p
            && *lsn < r
            && matches!(&e.record, Record::LoopDecided {
                agent: who,
                decision: LoopDecision::Hold { reason: dudes_proto::HoldReason::Paused },
                ..
            } if *who == agent)),
        "the Hold{{Paused}} decision lies between P and R"
    );
    assert!(
        !entries.iter().any(|(lsn, e)| *lsn < r
            && matches!(&e.record, Record::TurnIssued { agent: who, lsn_of_decision, .. }
                if *who == agent && *lsn_of_decision > p)),
        "no turn was decided between P and R"
    );

    // Step 6: the first decision after R is `Continue`, admitted through the budget gate.
    let (continued_at, trace) = entries
        .iter()
        .find_map(|(lsn, e)| match &e.record {
            Record::LoopDecided {
                agent: who,
                decision,
                gate_trace,
                ..
            } if *lsn > r && *who == agent => Some((*lsn, decision.clone(), gate_trace.clone())),
            _ => None,
        })
        .map(|(lsn, decision, trace)| {
            assert!(
                matches!(decision, LoopDecision::Continue { .. }),
                "{decision:?}"
            );
            (lsn, trace)
        })
        .expect("a decision after R");
    assert!(
        trace
            .iter()
            .any(|step| step.gate == dudes_proto::Gate::Budget),
        "the budget gate admitted the turn: {trace:?}"
    );

    // Step 7: that decision's turn is issued and spawned, and the stub saw exactly one more.
    let t2 = entries
        .iter()
        .find_map(|(_, e)| match &e.record {
            Record::TurnIssued {
                agent: who,
                turn,
                lsn_of_decision,
                ..
            } if *who == agent && *lsn_of_decision == continued_at => Some(*turn),
            _ => None,
        })
        .expect("a TurnIssued for that decision");
    assert!(
        entries.iter().any(|(lsn, e)| *lsn > r
            && matches!(&e.record, Record::TurnSpawned { turn, .. } if *turn == t2))
    );
    assert_eq!(spawns(), 2, "exactly one more stub spawn after R");
    let journal = serde_json::to_string(&entries).expect("journal JSON");
    for secret in [cookie.as_str(), csrf.as_str(), pause_wire.as_str()] {
        assert!(
            !journal.contains(secret),
            "raw browser credential in the journal"
        );
    }

    // Step 8: replay leaves the agent unpaused, and the index answers both keys.
    let daemon = boot(&settings, fake_seams(&settings)).expect("replay boot");
    let row = &daemon.world().agents[&agent];
    assert!(!row.paused && !row.halted, "replayed: {row:?}");
    assert_eq!(row.control_lsn, Some(r));
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    let (head, response) = post_control(
        addr,
        PAUSE,
        &cookie,
        &csrf,
        &control_body(&pause_wire, agent, None),
    );
    assert_eq!(applied(&head, &response, "paused"), p);
    let (head, response) = post_control(
        addr,
        RESUME,
        &cookie,
        &csrf,
        &control_body(&resume_wire, agent, Some(p)),
    );
    assert_eq!(applied(&head, &response, "resumed"), r);
    assert_eq!(
        fixture.records(),
        before,
        "the index answered; nothing was appended"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Every browser credential is checked before any effect: a wrong Host, a wrong or missing Origin,
/// an unknown cookie, a wrong or missing CSRF and an expired session each get the uniform empty
/// `403` on both routes, and the journal does not move. Control: the right credentials apply.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "six credential failures on both routes, the expired session and the control"
)]
fn browser_pause_resume_refuses_wrong_session_host_origin_and_csrf_without_effect() {
    let fixture = Fixture::new();
    let settings = control_settings(&fixture, true);
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = serde_json::to_vec(&control_body(&request_id(1), agent, None)).unwrap();
    let host = addr.to_string();
    let request =
        |path: &str, host: &str, origin: Option<&str>, cookie: &str, csrf: Option<&str>| {
            let mut head = format!("POST {path} HTTP/1.1\r\nHost: {host}\r\n");
            if let Some(origin) = origin {
                let _ = write!(head, "Origin: {origin}\r\n");
            }
            let _ = write!(head, "Cookie: dudes_floor_session={cookie}\r\n");
            if let Some(csrf) = csrf {
                let _ = write!(head, "X-Dudes-CSRF: {csrf}\r\n");
            }
            let _ = write!(
                head,
                "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let mut bytes = head.into_bytes();
            bytes.extend_from_slice(&body);
            bytes
        };
    let origin = format!("http://{host}");
    let unknown = "A".repeat(43);
    let before = fixture.records();
    for path in [PAUSE, RESUME] {
        for (label, bytes) in [
            (
                "wrong Host",
                request(path, "127.0.0.1:1", Some(&origin), &cookie, Some(&csrf)),
            ),
            (
                "wrong Origin",
                request(
                    path,
                    &host,
                    Some("http://127.0.0.1:1"),
                    &cookie,
                    Some(&csrf),
                ),
            ),
            (
                "missing Origin",
                request(path, &host, None, &cookie, Some(&csrf)),
            ),
            (
                "unknown cookie",
                request(path, &host, Some(&origin), &unknown, Some(&csrf)),
            ),
            (
                "wrong CSRF",
                request(path, &host, Some(&origin), &cookie, Some(&unknown)),
            ),
            (
                "missing CSRF",
                request(path, &host, Some(&origin), &cookie, None),
            ),
        ] {
            let (head, response) = raw_onboard(addr, &bytes);
            assert!(
                head.starts_with("HTTP/1.1 403 Forbidden"),
                "{path} {label}: {head}"
            );
            assert!(response.is_empty(), "{path} {label}: no envelope");
        }
    }
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let (expired_cookie, expired_csrf) = exchange(addr, &issue(&settings).bootstrap);
    clock.advance(30 * 60 * 1000);
    let (fresh_cookie, fresh_csrf) = exchange(addr, &issue(&settings).bootstrap);
    for path in [PAUSE, RESUME] {
        let (head, response) = post_control(
            addr,
            path,
            &expired_cookie,
            &expired_csrf,
            &control_body(&request_id(1), agent, None),
        );
        empty_refusal(&head, &response, "HTTP/1.1 403 Forbidden");
    }
    assert_eq!(
        fixture.records(),
        before,
        "an expired session appends nothing"
    );
    let _ = (cookie, csrf);

    let (head, response) = post_control(
        addr,
        PAUSE,
        &fresh_cookie,
        &fresh_csrf,
        &control_body(&request_id(1), agent, None),
    );
    applied(&head, &response, "paused");
    assert_eq!(controls_for(&fixture.records(), agent), (1, 0));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The first and last refusal reasons. An unknown agent and a retired one are `agent_absent`; a
/// `control_lsn` that is another agent's identity is `stale_control`. A control record written
/// before the attribution existed still sets the identity: presenting its position applies, and
/// presenting `null` over it is stale. Control: a live agent with the right identity applies.
#[test]
fn browser_pause_resume_refuses_unknown_retired_and_foreign_control_lsn() {
    let fixture = Fixture::new();
    let settings = control_settings(&fixture, true);
    let mut running =
        common::start(boot(&settings, fake_seams(&settings)).expect("boot the setup run"));
    let live = common::hire(&running.handle, fixture.state.path());
    let retired = common::hire(&running.handle, fixture.state.path());
    let other = common::hire(&running.handle, fixture.state.path());
    let older = common::hire(&running.handle, fixture.state.path());
    running
        .handle
        .ask(Ask::Retire(Box::new(RetireParams {
            agent: retired,
            reason: "done".to_owned(),
        })))
        .expect("retire");
    running.handle.ask(Ask::Pause(other)).expect("local pause");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    // The bytes a daemon built before the attribution wrote: no `browser` key.
    seed(
        &fixture,
        [Record::Pause {
            agent: older,
            browser: None,
        }],
    );
    let other_lsn = disk_control_lsn(&fixture, other).expect("the local pause");
    let older_lsn = disk_control_lsn(&fixture, older).expect("the seeded pause");
    let older_json = fixture
        .entries()
        .into_iter()
        .find(|(lsn, _)| *lsn == older_lsn)
        .map(|(_, e)| serde_json::to_value(&e.record).expect("record JSON"))
        .expect("seeded");
    assert!(older_json.get("browser").is_none(), "{older_json}");

    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let unknown = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("an agent id");
    assert!(
        !fixture.records().iter().any(|record| matches!(
            record,
            Record::AgentHired { id, .. } if *id == unknown
        )),
        "never hired"
    );
    let before = fixture.records();
    for (k, path, agent, lsn, reason) in [
        (1, PAUSE, unknown, None, "agent_absent"),
        (2, RESUME, unknown, None, "agent_absent"),
        (3, PAUSE, retired, None, "agent_absent"),
        (4, RESUME, retired, None, "agent_absent"),
        (5, PAUSE, live, Some(other_lsn), "stale_control"),
        (6, RESUME, older, None, "stale_control"),
    ] {
        let (head, response) = post_control(
            addr,
            path,
            &cookie,
            &csrf,
            &control_body(&request_id(k), agent, lsn),
        );
        refused(&head, &response, reason);
    }
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let (head, response) = post_control(
        addr,
        RESUME,
        &cookie,
        &csrf,
        &control_body(&request_id(7), older, Some(older_lsn)),
    );
    applied(&head, &response, "resumed");
    let (head, response) = post_control(
        addr,
        PAUSE,
        &cookie,
        &csrf,
        &control_body(&request_id(8), live, None),
    );
    applied(&head, &response, "paused");
    let records = fixture.records();
    assert_eq!(
        controls_for(&records, live),
        (1, 0),
        "control: live applies"
    );
    assert_eq!(controls_for(&records, older), (1, 1));
    assert_eq!(controls_for(&records, retired), (0, 0));
    assert_eq!(controls_for(&records, unknown), (0, 0));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Halt is unreachable from the browser. After a local halt, a browser resume is
/// `agent_halted` with no record; a browser pause applies, and a resume presenting its position
/// is still `agent_halted`. No path names halt: `POST /floor/halt` is byte for byte an unrouted
/// POST, and a `halt` or `action` member is an unknown member. Positive control: a local
/// `loop.resume` lifts the halt.
#[test]
fn browser_resume_cannot_lift_a_halt() {
    let fixture = Fixture::new();
    let settings = control_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    running.handle.ask(Ask::Halt(agent)).expect("local halt");
    let halt = disk_control_lsn(&fixture, agent).expect("the halt");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);

    let before = fixture.records();
    let (head, response) = post_control(
        addr,
        RESUME,
        &cookie,
        &csrf,
        &control_body(&request_id(1), agent, Some(halt)),
    );
    refused(&head, &response, "agent_halted");
    assert_eq!(fixture.records(), before);

    let (head, response) = post_control(
        addr,
        PAUSE,
        &cookie,
        &csrf,
        &control_body(&request_id(2), agent, Some(halt)),
    );
    let p = applied(&head, &response, "paused");
    let (head, response) = post_control(
        addr,
        RESUME,
        &cookie,
        &csrf,
        &control_body(&request_id(3), agent, Some(p)),
    );
    refused(&head, &response, "agent_halted");
    let records = fixture.records();
    assert_eq!(controls_for(&records, agent), (1, 0), "zero Resume records");
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::TurnIssued { .. })),
        "no turn"
    );

    let body = serde_json::to_vec(&control_body(&request_id(4), agent, Some(p))).unwrap();
    let halted = raw_post(addr, "/floor/halt", &cookie, &csrf, &body);
    let unrouted = raw_post(addr, "/floor/unrouted", &cookie, &csrf, &body);
    assert!(!unrouted.is_empty());
    assert_eq!(
        halted,
        unrouted,
        "no halt route: {}",
        String::from_utf8_lossy(&halted)
    );
    for member in ["halt", "action"] {
        let mut body = control_body(&request_id(5), agent, Some(p));
        body[member] = json!(if member == "halt" {
            json!(true)
        } else {
            json!("halt")
        });
        for path in [PAUSE, RESUME] {
            let (head, response) = post_control(addr, path, &cookie, &csrf, &body);
            assert!(
                head.starts_with("HTTP/1.1 400 Bad Request"),
                "{member} on {path}: {head}"
            );
            assert!(response.is_null());
        }
    }
    assert_eq!(fixture.records(), records, "nothing appended");

    running
        .handle
        .ask(Ask::Resume(agent))
        .expect("local resume");
    let world = folded(&fixture);
    let row = &world.agents[&agent];
    assert!(
        !row.halted && !row.paused,
        "positive control: the local resume lifts the halt"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The world a fresh replay of the journal on disk folds to.
fn folded(fixture: &Fixture) -> dudes_supervisor::World {
    let mut world = dudes_supervisor::World::default();
    for (lsn, entry) in fixture.entries() {
        world.apply(lsn, &entry);
    }
    world
}

/// Every applied body is single use. Resent under a new identity it is `stale_control`; resent
/// under its own it gets its original `200`; and after a later local change, a body that differs
/// from the current identity only by its position is still `stale_control`. No new record.
#[test]
fn browser_pause_resume_replayed_body_has_no_effect() {
    let fixture = Fixture::new();
    let settings = control_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = control_body(&request_id(1), agent, None);
    let (head, original) = post_control(addr, PAUSE, &cookie, &csrf, &body);
    applied(&head, &original, "paused");
    let applied_records = fixture.records();

    let mut replayed = body.clone();
    replayed["request_id"] = json!(request_id(2));
    let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &replayed);
    refused(&head, &response, "stale_control");
    let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(response, original, "its own identity gets its original 200");
    assert_eq!(fixture.records(), applied_records, "zero new records");

    running
        .handle
        .ask(Ask::Resume(agent))
        .expect("local resume");
    let local = fixture.records();
    replayed["request_id"] = json!(request_id(3));
    let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &replayed);
    refused(&head, &response, "stale_control");
    assert_eq!(
        fixture.records(),
        local,
        "only the position differs, and it is stale"
    );
    assert_eq!(controls_for(&local, agent), (1, 1));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The journal-derived index answers a same-key retry with its original result — even after
/// a later local change, and from a fresh session after a restart — and with no new record. The
/// same key with a changed payload, or sent to the other route, is an empty `409`.
#[test]
fn browser_pause_resume_same_key_retry_returns_original_and_changed_payload_conflicts() {
    let fixture = Fixture::new();
    let settings = control_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = control_body(&request_id(1), agent, None);
    let (head, original) = post_control(addr, PAUSE, &cookie, &csrf, &body);
    let p = applied(&head, &original, "paused");
    running
        .handle
        .ask(Ask::Resume(agent))
        .expect("local resume");
    running.handle.ask(Ask::Pause(agent)).expect("local pause");
    let later = fixture.records();

    let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(response, original, "after a later local change");
    let mut changed = body.clone();
    changed["control_lsn"] = json!(p.0.to_string());
    let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &changed);
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    let (head, response) = post_control(addr, RESUME, &cookie, &csrf, &body);
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    assert_eq!(fixture.records(), later, "no retry appended anything");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let daemon = boot(&settings, fake_seams(&settings)).expect("restart");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(response, original, "from a fresh session after a restart");
    let (head, response) = post_control(addr, RESUME, &cookie, &csrf, &body);
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    assert_eq!(fixture.records(), before);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// While a request is in flight: with the owner past its world check and before its append
/// permit, a same-key follower gets `202 pending`, a changed payload an empty `409`, and the same
/// key on the other route an empty `409` — the two routes share one namespace — all without the
/// writer. Then the owner's `200`. Two keys sent from one page view yield one record and one
/// `stale_control`.
#[test]
fn browser_pause_resume_concurrent_same_key_is_pending_and_two_keys_yield_one_record() {
    let fixture = Fixture::new();
    let settings = control_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let second = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = control_body(&request_id(1), agent, None);

    pause.arm(OnboardingPausePoint::BeforeControlPermit);
    let owner = {
        let (cookie, csrf, body) = (cookie.clone(), csrf.clone(), body.clone());
        std::thread::spawn(move || post_control(addr, PAUSE, &cookie, &csrf, &body))
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the owner is in flight"
    );
    // Each must answer while the owner still holds the writer. Each waits a bounded time; the
    // writer is released before any assertion, so a follower that queued behind it fails the
    // test rather than hanging it.
    let at_once = |path: &'static str, body: Value| {
        let (tx, rx) = mpsc::channel();
        let (cookie, csrf) = (cookie.clone(), csrf.clone());
        std::thread::spawn(move || {
            let _ = tx.send(post_control(addr, path, &cookie, &csrf, &body));
        });
        rx.recv_timeout(Duration::from_secs(3)).ok()
    };
    let follower = at_once(PAUSE, body.clone());
    let mut changed = body.clone();
    changed["agent"] = json!(second);
    let changed = at_once(PAUSE, changed);
    let crossed = at_once(RESUME, body.clone());
    pause.release();
    let (head, pending) = follower.expect("a same-key follower answers without the writer");
    assert!(head.starts_with("HTTP/1.1 202 Accepted"), "{head}");
    assert_eq!(
        pending,
        json!({ "request_id": request_id(1), "state": "pending" })
    );
    let (head, response) = changed.expect("a changed payload answers without the writer");
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    let (head, response) = crossed.expect("the other route answers without the writer");
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    let (head, response) = owner.join().expect("owner");
    applied(&head, &response, "paused");
    assert_eq!(controls_for(&fixture.records(), agent), (1, 0));

    let view = control_body(&request_id(2), second, None);
    let mut twin = view.clone();
    twin["request_id"] = json!(request_id(3));
    let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &view);
    applied(&head, &response, "paused");
    let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &twin);
    refused(&head, &response, "stale_control");
    assert_eq!(
        controls_for(&fixture.records(), second),
        (1, 0),
        "one record"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Consequence of the single writer: a local command that reaches the writer first is never overwritten. One arm
/// per operand of `stale_control`: a position that moved while the state came back (pause, then
/// resume, locally); a current position with the wrong state for the verb (a paused agent's
/// pause, an unpaused agent's resume); and a request whose append failed before its fsync, a
/// local change, and a same-key retry, which is refused.
#[test]
fn browser_pause_resume_local_winner_is_never_overwritten() {
    let fixture = Fixture::new();
    let settings = control_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let append_failure = daemon.append_failure();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let moved = common::hire(&running.handle, fixture.state.path());
    let paused = common::hire(&running.handle, fixture.state.path());
    let unpaused = common::hire(&running.handle, fixture.state.path());
    let crashed = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);

    // The page saw `moved` with no control record; locally it is paused and resumed since.
    running.handle.ask(Ask::Pause(moved)).expect("local pause");
    running
        .handle
        .ask(Ask::Resume(moved))
        .expect("local resume");
    running.handle.ask(Ask::Pause(paused)).expect("local pause");
    let paused_lsn = disk_control_lsn(&fixture, paused);
    running
        .handle
        .ask(Ask::Pause(unpaused))
        .expect("local pause");
    running
        .handle
        .ask(Ask::Resume(unpaused))
        .expect("local resume");
    let unpaused_lsn = disk_control_lsn(&fixture, unpaused);
    let before = fixture.records();
    for (k, path, agent, lsn, what) in [
        (1, PAUSE, moved, None, "the position moved"),
        (2, PAUSE, paused, paused_lsn, "already paused"),
        (3, RESUME, unpaused, unpaused_lsn, "not paused"),
    ] {
        let (head, response) = post_control(
            addr,
            path,
            &cookie,
            &csrf,
            &control_body(&request_id(k), agent, lsn),
        );
        assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{what}: {head}");
        assert_eq!(response["reason"], "stale_control", "{what}: {response}");
    }
    assert_eq!(fixture.records(), before, "no local record was overwritten");

    append_failure.fail_nth(NonZeroUsize::new(1).unwrap());
    let body = control_body(&request_id(4), crashed, None);
    let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &body);
    empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
    assert_eq!(controls_for(&fixture.records(), crashed), (0, 0));
    running
        .handle
        .ask(Ask::Pause(crashed))
        .expect("local pause");
    running
        .handle
        .ask(Ask::Resume(crashed))
        .expect("local resume");
    let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &body);
    refused(&head, &response, "stale_control");
    assert_eq!(
        controls_for(&fixture.records(), crashed),
        (1, 1),
        "only the local records"
    );
    let _ = (paused, unpaused);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Across restarts, at four cuts: after the pause's fsync and before its fold; after its fold;
/// after the resume's fsync and before its `ClearedCause`; after the resumed turn's `TurnIssued`
/// fsync and before Spawn. Each replays exactly one pause, one resume where it was sent, one turn
/// and one Spawn, and the same key answers from the index.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "four restart cuts share one scenario and one assertion shape"
)]
fn browser_pause_resume_restart_cut_points_replay_one_record() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Cut {
        PauseFsync,
        PauseFold,
        ResumeFsync,
        TurnIssuedFsync,
    }
    for cut in [
        Cut::PauseFsync,
        Cut::PauseFold,
        Cut::ResumeFsync,
        Cut::TurnIssuedFsync,
    ] {
        let fixture = Fixture::new();
        let settings = control_settings(&fixture, true);
        let host = SharedHost::new();
        let run = |host: &SharedHost| {
            boot(
                &settings,
                seams(
                    &settings,
                    host.clone(),
                    Arc::new(ScriptedRunner::default()),
                    Arc::new(TestClock::new()),
                    Witness::default(),
                ),
            )
            .expect("boot Floor")
        };
        let daemon = run(&host);
        let failure = daemon.onboarding_failure();
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let agent = common::hire(&running.handle, fixture.state.path());
        let other = common::hire(&running.handle, fixture.state.path());
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let pause_body = control_body(&request_id(1), agent, None);
        if cut == Cut::PauseFsync {
            failure.arm(OnboardingFailurePoint::Control);
        }
        let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &pause_body);
        let mut resume_body = None;
        if cut == Cut::PauseFsync {
            empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
            let (head, pending) = post_control(addr, PAUSE, &cookie, &csrf, &pause_body);
            assert!(head.starts_with("HTTP/1.1 202 Accepted"), "{head}");
            assert_eq!(
                pending["state"], "pending",
                "an uncertain append keeps its key"
            );
            let fresh = control_body(&request_id(2), other, None);
            let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &fresh);
            empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
        } else {
            let p = applied(&head, &response, "paused");
            if matches!(cut, Cut::ResumeFsync | Cut::TurnIssuedFsync) {
                // A goal set while paused is held; only the resume can issue its turn.
                common::set_goal(
                    &running.handle,
                    common::goal_params(agent, &[true_program()]),
                );
                failure.arm(if cut == Cut::ResumeFsync {
                    OnboardingFailurePoint::Control
                } else {
                    OnboardingFailurePoint::TurnIssued
                });
                let body = control_body(&request_id(3), agent, Some(p));
                let (head, response) = post_control(addr, RESUME, &cookie, &csrf, &body);
                if cut == Cut::ResumeFsync {
                    empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
                } else {
                    applied(&head, &response, "resumed");
                }
                resume_body = Some(body);
            }
        }
        assert_eq!(
            host.spawned().len(),
            0,
            "{cut:?}: the cut is before any Spawn"
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

        let daemon = run(&host);
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        if resume_body.is_some() {
            fixture.await_record("the resumed turn's spawn", |record| {
                matches!(record, Record::TurnSpawned { agent: who, .. } if *who == agent)
                    .then_some(())
            });
        }
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &pause_body);
        let p = applied(&head, &response, "paused");
        let mut resumes = 0;
        if let Some(body) = &resume_body {
            let (head, response) = post_control(addr, RESUME, &cookie, &csrf, body);
            let r = applied(&head, &response, "resumed");
            assert!(r > p, "{cut:?}");
            resumes = 1;
            let issued: Vec<Lsn> = fixture
                .records()
                .iter()
                .filter_map(|record| match record {
                    Record::TurnIssued {
                        agent: who,
                        lsn_of_decision,
                        ..
                    } if *who == agent => Some(*lsn_of_decision),
                    _ => None,
                })
                .collect();
            assert_eq!(issued.len(), 1, "{cut:?}: one turn: {issued:?}");
            assert!(issued[0] > r, "{cut:?}: decided after the resume");
            assert_eq!(host.spawned().len(), 1, "{cut:?}: one Spawn");
            assert_eq!(
                fixture
                    .records()
                    .iter()
                    .filter(|record| matches!(record, Record::TurnSpawned { agent: who, .. } if *who == agent))
                    .count(),
                1,
                "{cut:?}"
            );
        } else {
            assert_eq!(host.spawned().len(), 0, "{cut:?}: nothing to spawn");
        }
        let records = fixture.records();
        assert_eq!(controls_for(&records, agent), (1, resumes), "{cut:?}");
        assert_eq!(controls_for(&records, other), (0, 0), "{cut:?}");
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

/// A correctly identified live session passes the world check, is revoked before its append
/// permit, and gets the uniform empty `403` with no `Pause`, `Resume`, `LoopDecided` or
/// `TurnIssued`. In the opposite order, revocation waits for the held permit's fsync and the
/// request gets its `200`.
///
/// The two mutants turn this red: the permit match omitting an attributed `Pause` or
/// `Resume`, and `permit` returning without consulting the registry. Deleting only the `revoked`
/// flag check is not a mutant: revocation also removes the session from the registry.
#[test]
fn browser_pause_resume_revocation_before_the_permit_is_forbidden() {
    let fixture = Fixture::new();
    let settings = control_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let authorizer = daemon.floor_session_authorizer().expect("Floor sessions");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();

    pause.arm(OnboardingPausePoint::BeforeControlPermit);
    let losing = {
        let (cookie, csrf) = (cookie.clone(), csrf.clone());
        std::thread::spawn(move || {
            post_control(
                addr,
                PAUSE,
                &cookie,
                &csrf,
                &control_body(&request_id(1), agent, None),
            )
        })
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "admitted, world-checked, before the permit"
    );
    assert!(
        authorizer.revoke_for_test(&cookie),
        "the live session is revoked"
    );
    pause.release();
    let (head, response) = losing.join().expect("revoked request");
    empty_refusal(&head, &response, "HTTP/1.1 403 Forbidden");
    let after_revoke = fixture.records();
    assert!(
        !after_revoke[before.len()..].iter().any(|record| matches!(
            record,
            Record::Pause { .. }
                | Record::Resume { .. }
                | Record::LoopDecided { .. }
                | Record::TurnIssued { .. }
        )),
        "{:?}",
        &after_revoke[before.len()..]
    );

    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    pause.arm(OnboardingPausePoint::AfterControlPermit);
    let owner = {
        let (cookie, csrf) = (cookie.clone(), csrf.clone());
        std::thread::spawn(move || {
            post_control(
                addr,
                PAUSE,
                &cookie,
                &csrf,
                &control_body(&request_id(1), agent, None),
            )
        })
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the append holds its permit"
    );
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let revoke = authorizer.clone();
    let revoker = std::thread::spawn(move || {
        started_tx.send(()).expect("started signal");
        done_tx
            .send(revoke.revoke_for_test(&cookie))
            .expect("revoke result");
    });
    started_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("revoker started");
    assert!(
        done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "revocation waits while the append permit is held"
    );
    pause.release();
    assert!(
        done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("revocation completed")
    );
    revoker.join().expect("revoker thread");
    let (head, response) = owner.join().expect("winning request");
    applied(&head, &response, "paused");
    let _ = csrf;
    assert_eq!(controls_for(&fixture.records(), agent), (1, 0));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// No clock read happens under the registry lock: session A holds its pause's append permit
/// — and so the registry lock — while session B's pause waits for admission and the test clock
/// crosses the absolute expiry. A's held permit wins; B gets the uniform empty `403` and appends
/// nothing; and no clock read happens while A holds the lock.
#[test]
fn browser_pause_resume_expiry_waiting_on_registry_lock_is_forbidden() {
    let fixture = Fixture::new();
    let settings = control_settings(&fixture, true);
    let clock = Arc::new(ProbeClock {
        inner: TestClock::ticking(1_800_000_000_000, 0),
        armed: Mutex::new(None),
    });
    let daemon = boot(
        &settings,
        Seams {
            host: SharedHost::new(),
            pty_host: Box::new(duded::ports::ClosedPtyHost),
            predicates: Arc::new(ScriptedRunner::default()),
            clock: Arc::clone(&clock),
            rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
            resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
            adapters: [Box::new(StubAdapter) as Box<dyn Adapter>]
                .into_iter()
                .collect(),
            children: ChildEnds::new(),
            witness: Witness::default(),
        },
    )
    .expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let a = common::hire(&running.handle, fixture.state.path());
    let b = common::hire(&running.handle, fixture.state.path());
    let (a_cookie, a_csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (b_cookie, b_csrf) = exchange(addr, &issue(&settings).bootstrap);

    pause.arm(OnboardingPausePoint::AfterControlPermit);
    let holder = std::thread::spawn(move || {
        post_control(
            addr,
            PAUSE,
            &a_cookie,
            &a_csrf,
            &control_body(&request_id(1), a, None),
        )
    });
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "A holds its append permit"
    );
    let probe = clock.arm();
    let (sent_tx, sent_rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let body = serde_json::to_vec(&control_body(&request_id(2), b, None)).unwrap();
        post_bytes(addr, PAUSE, &b_cookie, &b_csrf, &body, Some(sent_tx))
    });
    sent_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("B is sent while A holds the lock");
    let sampled_while_held = probe.recv_timeout(Duration::from_millis(500)).is_ok();
    clock.inner.advance(8 * 60 * 60 * 1000);
    pause.release();
    let (head, winner) = holder.join().expect("A");
    applied(&head, &winner, "paused");
    let (head, response) = waiter.join().expect("B");
    assert!(
        probe.recv_timeout(Duration::from_secs(2)).is_ok() || sampled_while_held,
        "positive control: the clock was read once armed"
    );
    assert!(
        !sampled_while_held,
        "session validity was sampled while another session held the registry lock"
    );
    empty_refusal(&head, &response, "HTTP/1.1 403 Forbidden");
    let records = fixture.records();
    assert_eq!(controls_for(&records, a), (1, 0));
    assert_eq!(
        controls_for(&records, b),
        (0, 0),
        "the expired waiter appended nothing"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// An uncertain local write fences the daemon: a durable pause's same-key retry is still served
/// from the index, with its original result and a changed payload's conflict, while a fresh
/// request gets an empty `503`, appends nothing, and releases its key.
#[test]
fn browser_pause_resume_uncertain_local_write_fences_fresh_requests_and_serves_durable_retries() {
    let fixture = Fixture::new();
    let settings = control_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let failure = daemon.onboarding_failure();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let first = common::hire(&running.handle, fixture.state.path());
    let second = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let durable = control_body(&request_id(1), first, None);
    let (head, original) = post_control(addr, PAUSE, &cookie, &csrf, &durable);
    let p = applied(&head, &original, "paused");

    failure.arm(OnboardingFailurePoint::LocalGoal);
    let local = running
        .handle
        .ask(Ask::SetGoal(Box::new(common::goal_params(
            second,
            &[true_program()],
        ))));
    assert!(
        local.is_err(),
        "the local goal is durable and then fails its fold"
    );
    let fenced = fixture.records();

    for attempt in 0..2 {
        let (head, retry) = post_control(addr, PAUSE, &cookie, &csrf, &durable);
        assert!(
            head.starts_with("HTTP/1.1 200 OK"),
            "retry {attempt} while fenced: {head}"
        );
        assert_eq!(retry, original);
    }
    let mut changed = durable.clone();
    changed["control_lsn"] = json!(p.0.to_string());
    let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &changed);
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    let fresh = control_body(&request_id(2), second, None);
    for _ in 0..2 {
        let (head, response) = post_control(addr, PAUSE, &cookie, &csrf, &fresh);
        empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
    }
    assert_eq!(fixture.records(), fenced, "nothing appended while fenced");
    assert_eq!(controls_for(&fixture.records(), second), (0, 0));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The request bounds refuse before any effect: 513 bytes (`413`, before a byte is read); an unknown, a
/// duplicate, a nested and a missing member; a numeric `control_lsn`; a noncanonical request
/// identity, agent or `control_lsn`. Controls: a valid body padded to exactly 512 bytes applies,
/// and so does the minimal one.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "every request bound is one row of a single table beside its two controls"
)]
fn browser_pause_resume_bounds_and_json_shape_refuse_before_effect() {
    let fixture = Fixture::new();
    let settings = control_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let second = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let id = request_id(1);
    let alias = agent.to_string().replacen('0', "O", 1);
    assert_ne!(alias, agent.to_string(), "the alias spelling differs");
    let refusals: Vec<(&str, String)> = vec![
        (
            "unknown member",
            format!(r#"{{"request_id":"{id}","agent":"{agent}","control_lsn":null,"extra":1}}"#),
        ),
        (
            "duplicate member",
            format!(
                r#"{{"request_id":"{id}","agent":"{agent}","control_lsn":null,"agent":"{agent}"}}"#
            ),
        ),
        (
            "nested member",
            format!(r#"{{"request_id":"{id}","agent":{{"id":"{agent}"}},"control_lsn":null}}"#),
        ),
        (
            "missing control_lsn",
            format!(r#"{{"request_id":"{id}","agent":"{agent}"}}"#),
        ),
        (
            "numeric control_lsn",
            format!(r#"{{"request_id":"{id}","agent":"{agent}","control_lsn":5}}"#),
        ),
        (
            "zero control_lsn",
            format!(r#"{{"request_id":"{id}","agent":"{agent}","control_lsn":"0"}}"#),
        ),
        (
            "leading-zero control_lsn",
            format!(r#"{{"request_id":"{id}","agent":"{agent}","control_lsn":"05"}}"#),
        ),
        (
            "signed control_lsn",
            format!(r#"{{"request_id":"{id}","agent":"{agent}","control_lsn":"+5"}}"#),
        ),
        (
            "21-digit control_lsn",
            format!(
                r#"{{"request_id":"{id}","agent":"{agent}","control_lsn":"{}"}}"#,
                "1".repeat(21)
            ),
        ),
        (
            "noncanonical request_id",
            format!(
                r#"{{"request_id":"{}B","agent":"{agent}","control_lsn":null}}"#,
                "A".repeat(42)
            ),
        ),
        (
            "short request_id",
            format!(
                r#"{{"request_id":"{}","agent":"{agent}","control_lsn":null}}"#,
                "A".repeat(42)
            ),
        ),
        (
            "alias-letter agent",
            format!(r#"{{"request_id":"{id}","agent":"{alias}","control_lsn":null}}"#),
        ),
        ("an array", format!(r#"["{id}","{agent}",null]"#)),
    ];
    let before = fixture.records();
    for path in [PAUSE, RESUME] {
        for (label, body) in &refusals {
            let (head, response) = post_bytes(addr, path, &cookie, &csrf, body.as_bytes(), None);
            assert!(
                head.starts_with("HTTP/1.1 400 Bad Request"),
                "{path} {label}: {head}"
            );
            assert!(response.is_null(), "{path} {label}: {response}");
        }
        let host = addr.to_string();
        let oversize = format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: dudes_floor_session={cookie}\r\nX-Dudes-CSRF: {csrf}\r\nContent-Type: application/json\r\nContent-Length: 513\r\n\r\n"
        );
        let (head, body) = raw_onboard(addr, oversize.as_bytes());
        assert!(
            head.starts_with("HTTP/1.1 413"),
            "{path}: refused before a byte is read: {head}"
        );
        assert!(body.is_empty());
    }
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let compact = serde_json::to_string(&control_body(&request_id(2), agent, None)).unwrap();
    let padded = format!("{}{}", compact, " ".repeat(512 - compact.len()));
    assert_eq!(padded.len(), 512);
    let (head, response) = post_bytes(addr, PAUSE, &cookie, &csrf, padded.as_bytes(), None);
    applied(&head, &response, "paused");
    let (head, response) = post_control(
        addr,
        PAUSE,
        &cookie,
        &csrf,
        &control_body(&request_id(3), second, None),
    );
    applied(&head, &response, "paused");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// With control off — the default — neither route exists: each response is byte for byte an
/// unrouted POST's, and no snapshot carries the `control` sibling. Control: the same daemon
/// configuration with control on routes both and sends the sibling. And the operator's switch
/// loads with no budget at all.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "two switch settings and every load case share one table"
)]
fn browser_pause_resume_is_absent_when_not_enabled() {
    for enabled in [false, true] {
        let fixture = Fixture::new();
        let settings = control_settings(&fixture, enabled);
        let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let agent = common::hire(&running.handle, fixture.state.path());
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let (view, _) = snapshot(addr, &cookie, &csrf);
        let unrouted = raw_post(
            addr,
            "/floor/unrouted",
            &cookie,
            &csrf,
            &serde_json::to_vec(&control_body(&request_id(1), agent, None)).unwrap(),
        );
        assert!(!unrouted.is_empty());
        for (k, path) in [(1, PAUSE), (2, RESUME)] {
            let body = serde_json::to_vec(&control_body(
                &request_id(k),
                agent,
                disk_control_lsn(&fixture, agent),
            ))
            .unwrap();
            let routed = raw_post(addr, path, &cookie, &csrf, &body);
            if enabled {
                assert_ne!(routed, unrouted, "control: {path} is a route");
                assert!(
                    String::from_utf8_lossy(&routed).starts_with("HTTP/1.1 200 OK"),
                    "{}",
                    String::from_utf8_lossy(&routed)
                );
            } else {
                assert_eq!(
                    routed,
                    unrouted,
                    "{path} disabled is exactly unrouted: {}",
                    String::from_utf8_lossy(&routed)
                );
            }
        }
        if enabled {
            assert_eq!(view["control"], json!({}));
            assert_eq!(controls_for(&fixture.records(), agent), (1, 1));
        } else {
            assert!(view.get("control").is_none(), "{view}");
            assert_eq!(controls_for(&fixture.records(), agent), (0, 0));
        }
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }

    let root = tempfile::tempdir().expect("operator config root");
    let config = root.path().join("config.toml");
    let valid = "[budget]\nfloor_usd_per_day = 10\n[budget.per_turn_floor]\nusd = 0.01\n";
    std::fs::write(&config, valid).expect("no control table");
    let control = |config| load_operator(config).expect("valid").control;
    assert_eq!(control(&config), FloorControl::Off, "absent is off");
    std::fs::write(
        &config,
        format!("{valid}[floor.control]\nenabled = false\n"),
    )
    .expect("off");
    assert_eq!(control(&config), FloorControl::Off);
    std::fs::write(&config, format!("{valid}[floor.control]\nenabled = true\n")).expect("on");
    assert_eq!(
        control(&config),
        FloorControl::On {
            goal: false,
            retire: false,
            renew: false,
        },
        "control: a positive ceiling and reserve admit the switch"
    );
    // Budgets are optional: control needs none of them.
    for (label, budget) in [
        ("no budget", ""),
        ("no floor ceiling", "[budget.per_turn_floor]\nusd = 0.01\n"),
        (
            "a zero floor ceiling",
            "[budget]\nfloor_usd_per_day = 0\n[budget.per_turn_floor]\nusd = 0.01\n",
        ),
        ("no dollar reserve", "[budget]\nfloor_usd_per_day = 10\n"),
    ] {
        std::fs::write(
            &config,
            format!("{budget}[floor.control]\nenabled = true\n"),
        )
        .expect("config");
        assert_eq!(
            control(&config),
            FloorControl::On {
                goal: false,
                retire: false,
                renew: false,
            },
            "{label} loads"
        );
    }
    std::fs::write(
        &config,
        format!("{valid}[floor.control]\nenabled = true\nhalt = true\n"),
    )
    .expect("unknown key");
    assert!(load_operator(&config).is_err(), "an unknown key is refused");
}

/// On an attached agent paused by the browser: its `PreToolUse` hook gets `Deny`, encoded
/// through the real hook command as `permissionDecision:"deny"` with exit 2; its
/// `UserPromptSubmit` hook gets the same `Deny`, encoded as `decision:"block"` with exit 2; and no
/// turn is issued. Control: before the pause, the same prompt hook is answered with silence.
#[test]
fn browser_pause_on_attached_agent_refuses_prompts_and_tools_and_issues_nothing() {
    use common::hook_shim::{
        RecordingResolver, generated_handlers, handler, minted_token, payload, spawn_handler,
    };
    let fixture = Fixture::new();
    let settings = control_settings(&fixture, true);
    let parts = fake_seams(&settings);
    let tokens = parts.rng.tokens();
    let mut running = common::start(boot(&settings, parts).expect("boot the setup run"));
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[true_program()]),
    );
    let token = minted_token(&fixture, &tokens, agent);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    seed(
        &fixture,
        [Record::Attached {
            agent,
            by: dudes_proto::Actor::System,
            hook_port: None,
            provenance: None,
        }],
    );

    // The operator's own calls resolve as the shipped daemon's do; the hooks, as the agent's.
    let resolver = RecordingResolver::new(PeerProvenance::Unresolvable);
    let fixture_parts = seams_with(
        &settings,
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
        Arc::clone(&resolver) as Arc<dyn PeerResolver>,
    );
    let daemon = boot(&settings, fixture_parts).expect("boot Floor");
    let socket = daemon.socket().to_path_buf();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    resolver.set(PeerProvenance::Agent(agent));
    let generated = generated_handlers(agent, fixture.state.path());
    let (_, unpaused) = spawn_handler(
        handler(&generated, "UserPromptSubmit"),
        &socket,
        &token,
        &payload("UserPromptSubmit"),
    );
    assert_eq!(unpaused.status.code(), Some(0), "control: {unpaused:?}");
    let context: Value = serde_json::from_slice(&unpaused.stdout).unwrap();
    assert_eq!(
        context["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let teaching = context["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert_eq!(teaching.len(), 432);
    assert!(teaching.starts_with("\n\n[Memory: read with `dudes memory read`"));
    assert!(teaching.contains("\n\n[Mail: start a conversation"));

    let (head, response) = post_control(
        addr,
        PAUSE,
        &cookie,
        &csrf,
        &control_body(&request_id(1), agent, None),
    );
    let p = applied(&head, &response, "paused");

    let (_, tool) = spawn_handler(
        handler(&generated, "PreToolUse"),
        &socket,
        &token,
        &payload("PreToolUse"),
    );
    assert_eq!(tool.status.code(), Some(2), "{tool:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&tool.stdout).unwrap(),
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": "the loop is paused"
        }})
    );
    let (_, prompt) = spawn_handler(
        handler(&generated, "UserPromptSubmit"),
        &socket,
        &token,
        &payload("UserPromptSubmit"),
    );
    assert_eq!(prompt.status.code(), Some(2), "{prompt:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&prompt.stdout).unwrap(),
        json!({"decision": "block", "reason": "the loop is paused"})
    );
    let _ = snapshot(addr, &cookie, &csrf);
    assert!(
        !after(&fixture, p)
            .iter()
            .any(|(_, e)| matches!(&e.record, Record::TurnIssued { .. })),
        "no turn is issued"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

// --- Change one agent's goal ------------------------------------------------------

const GOAL_ROUTE: &str = "/floor/goal";
const NEW_TEXT: &str = "rewrite the parser as a table";

/// Floor settings with goal change on or off. Pause and resume stay on either way, as goal change
/// requires.
fn goal_settings(fixture: &Fixture, enabled: bool) -> duded::Settings {
    let mut settings = control_settings(fixture, true);
    if enabled {
        settings.floor_control = FloorControl::On {
            goal: true,
            retire: false,
            renew: false,
        };
    }
    settings
}

fn goal_body(request_id: &str, agent: AgentId, goal: GoalId, text: &str) -> Value {
    json!({ "request_id": request_id, "agent": agent, "goal": goal, "text": text })
}

fn post_goal(addr: SocketAddr, cookie: &str, csrf: &str, body: &Value) -> (String, Value) {
    post_bytes(
        addr,
        GOAL_ROUTE,
        cookie,
        csrf,
        &serde_json::to_vec(body).expect("goal JSON"),
        None,
    )
}

/// A `200` naming the new goal; returns it.
fn changed(head: &str, response: &Value) -> GoalId {
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {response}");
    assert!(head.contains("Cache-Control: no-store"), "{head}");
    assert_eq!(response["state"], "changed", "{response}");
    GoalId::parse(response["goal"].as_str().expect("a goal id")).expect("a canonical goal id")
}

/// Every browser-attributed `GoalSet` naming `agent`, with its position, in journal order.
fn browser_goals(fixture: &Fixture, agent: AgentId) -> Vec<(Lsn, Goal, BrowserGoal)> {
    fixture
        .entries()
        .into_iter()
        .filter_map(|(lsn, entry)| match entry.record {
            Record::GoalSet {
                agent: owner,
                goal,
                browser_goal: Some(attribution),
                ..
            } if owner == agent => Some((lsn, goal, *attribution)),
            _ => None,
        })
        .collect()
}

/// The request digest, recomputed here rather than taken from the daemon.
fn expected_goal_digest(agent: AgentId, goal: GoalId, text: &str) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"dudes.goal.v1\0");
    for (field, value) in [
        ("agent", agent.to_string()),
        ("goal", goal.to_string()),
        ("text", text.to_owned()),
    ] {
        hasher.update(u16::try_from(field.len()).unwrap().to_be_bytes());
        hasher.update(field.as_bytes());
        hasher.update(u32::try_from(value.len()).unwrap().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.finalize().into()
}

/// Hire through the loop, pause locally, then set a goal: the control gate holds it, with no turn.
fn paused_with_goal(running: &common::Running, fixture: &Fixture) -> (AgentId, GoalId) {
    let agent = common::hire(&running.handle, fixture.state.path());
    running.handle.ask(Ask::Pause(agent)).expect("local pause");
    let goal = common::set_goal(
        &running.handle,
        common::goal_params(agent, &[true_program()]),
    );
    (agent, goal)
}

#[test]
fn browser_goal_refuses_a_legacy_claimed_predicate_before_append() {
    browser_goal_refuses_a_legacy_predicate(dudes_types::CardStatus::Claimed, "predicate_claimed");
}

/// A legacy goal waiting for Todo could complete on another agent's retirement alone, so its text
/// change is refused like a Claimed one.
#[test]
fn browser_goal_refuses_a_legacy_todo_predicate_before_append() {
    browser_goal_refuses_a_legacy_predicate(dudes_types::CardStatus::Todo, "predicate_todo");
}

fn browser_goal_refuses_a_legacy_predicate(status: dudes_types::CardStatus, reason: &str) {
    let fixture = Fixture::new();
    let mut settings = goal_settings(&fixture, true);
    settings.floor_control = FloorControl::On {
        goal: true,
        retire: false,
        renew: true,
    };
    let mut setup = common::start(boot(&settings, fake_seams(&settings)).expect("setup"));
    let (agent, goal) = paused_with_goal(&setup, &fixture);
    assert_eq!(setup.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    let mut legacy = fixture
        .records()
        .into_iter()
        .find(
            |record| matches!(record, Record::GoalSet { goal: current, .. } if current.id == goal),
        )
        .unwrap();
    let Record::GoalSet { goal: current, .. } = &mut legacy else {
        unreachable!()
    };
    let legacy_goal = GoalId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap();
    current.id = legacy_goal;
    NEW_TEXT.clone_into(&mut current.text);
    current.predicate = Predicate::BoardState {
        card: dudes_types::CardId::NIL,
        status,
    };
    let mut source = legacy.clone();
    let goal = GoalId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAX").unwrap();
    if let Record::GoalSet { goal: current, .. } = &mut source {
        current.id = goal;
    }
    let Record::GoalSet { browser_goal, .. } = &mut legacy else {
        unreachable!()
    };
    let mut identity = [0; 32];
    identity[0] = 4;
    *browser_goal = Some(Box::new(BrowserGoal {
        kind: dudes_proto::record::BrowserGoalKind::Change,
        request_id: Hex32::new(identity),
        request_digest: Hex32::new(expected_goal_digest(agent, goal, NEW_TEXT)),
        replaced_goal: goal,
        reserved: Spend::default(),
        session_digest: Hex32::new([9; 32]),
        check_chosen: false,
    }));
    seed(&fixture, [source, legacy]);
    let daemon = boot(&settings, fake_seams(&settings)).expect("replay legacy goal");
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    let (head, response) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_body(&request_id(1), agent, goal, NEW_TEXT),
    );
    assert_eq!(
        changed(&head, &response),
        legacy_goal,
        "durable retry truth precedes the predicate admission rule"
    );
    let (head, response) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_body(&request_id(2), agent, legacy_goal, NEW_TEXT),
    );
    refused(&head, &response, reason);
    let (head, response) = post_renew(
        addr,
        &cookie,
        &csrf,
        &renew_body(&request_id(3), agent, legacy_goal, SHOWN),
    );
    refused(&head, &response, reason);
    let (head, response) = post_renew(
        addr,
        &cookie,
        &csrf,
        &renew_body(&request_id(1), agent, goal, SHOWN),
    );
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    assert_eq!(fixture.records(), before);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The newest turn issued for `agent`.
fn newest_turn(fixture: &Fixture, agent: AgentId) -> TurnId {
    fixture
        .records()
        .into_iter()
        .rev()
        .find_map(|record| match record {
            Record::TurnIssued {
                agent: who, turn, ..
            } if who == agent => Some(turn),
            _ => None,
        })
        .expect("a turn was issued")
}

/// End a fake host's turn as its child would, having cost `usd`, or with no usage at all.
fn end_turn(handle: &duded::Handle, turn: TurnId, usd: Option<Usd>) {
    handle.input(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: usd.map(|usd| {
            ScopedUsage::Turn(Usage {
                input: Tokens::ZERO,
                output: Tokens::ZERO,
                cache_read: Tokens::ZERO,
                cache_creation: Tokens::ZERO,
                usd: Some(usd),
            })
        }),
        model_usage: std::collections::BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: None,
    });
}

/// Block until an entry `f` selects is on the disk, and return its position.
fn await_entry(fixture: &Fixture, what: &str, f: impl Fn(Lsn, &Record) -> bool) -> Lsn {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some((lsn, _)) = fixture
            .entries()
            .into_iter()
            .find(|(lsn, entry)| f(*lsn, &entry.record))
        {
            return lsn;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no {what} within 10s: {:?}",
            fixture
                .records()
                .iter()
                .map(Record::kind)
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Resend one goal change until it applies, for at most three seconds; each refusal on the way
/// has no effect. Returns the `200`'s new goal.
fn apply_goal_when_admitted(addr: SocketAddr, cookie: &str, csrf: &str, body: &Value) -> GoalId {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let (head, response) = post_goal(addr, cookie, csrf, body);
        if head.starts_with("HTTP/1.1 200 OK") {
            return changed(&head, &response);
        }
        assert_eq!(response["reason"], "turn_open", "{head} {response}");
        assert!(
            std::time::Instant::now() < deadline,
            "the change stayed refused: {response}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The primary test, stub-backed and read from the replayed journal, in its seven
/// ordered steps: a change is refused while T1 runs; after T1's end, its verdict and `Done`, one
/// applies; the new goal keeps G1's predicate and carries exactly what G1 had left; its own
/// decision is `Continue`, although G1 passed, and not the `Exhausted` that reading G1's spend
/// against G2's ceiling would give; that decision's turn carries the new text to the stub child;
/// G2 ends `Done` only on its own verdict; and replay keeps G2 and answers the key.
///
/// Mutants: the spend push for G2 before the change's decision is dropped (step 4 decides
/// `Exhausted`); the carry-over copies G1's hop cap or ceiling (step 3).
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end stub scenario asserts its seven ordered steps on one journal"
)]
fn browser_goal_change_is_finite_bounded_and_observed() {
    let fixture = Fixture::new();
    let capture = tempfile::tempdir().expect("task capture directory");
    let hold = capture.path().join("release-first-turn");
    let mut settings = goal_settings(&fixture, true);
    settings.daemon_env.push(EnvVar::new(HELPER_ROLE, "1"));
    settings
        .daemon_env
        .push(EnvVar::new(TASK_CAPTURE, capture.path().as_os_str()));
    settings
        .daemon_env
        .push(EnvVar::new(STUB_HOLD, hold.as_os_str()));
    let daemon = boot(&settings, real_stub_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let ceiling = Usd::from_micros(150_000);
    let g1 = common::set_goal(
        &running.handle,
        GoalSetParams {
            hop_cap: 4,
            ..common::goal_params_with_budget(agent, &[true_program()], ceiling)
        },
    );
    let t1 = fixture.await_record("T1 spawned", |record| match record {
        Record::TurnSpawned {
            agent: who, turn, ..
        } if *who == agent => Some(*turn),
        _ => None,
    });
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (view, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(view["goal"], json!({ "max_text_bytes": 2048 }));

    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::TurnEnded { turn, .. } if *turn == t1)),
        "T1 is still running"
    );
    let (head, response) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_body(&request_id(1), agent, g1, NEW_TEXT),
    );
    refused(&head, &response, "turn_open");
    assert!(browser_goals(&fixture, agent).is_empty());

    std::fs::write(&hold, b"").expect("release T1");
    fixture.await_record("G1 met", |record| {
        matches!(
            record,
            Record::LoopDecided { agent: who, decision: LoopDecision::Done, .. } if *who == agent
        )
        .then_some(())
    });
    let t1_usd = fixture
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::TurnEnded { turn, usage, .. } if turn == t1 => usage.and_then(|u| u.usd),
            _ => None,
        })
        .expect("T1's known cost");
    assert!(t1_usd.micros() * 2 >= ceiling.micros(), "{t1_usd:?}");
    let (head, response) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_body(&request_id(2), agent, g1, NEW_TEXT),
    );
    let g2 = changed(&head, &response);
    assert_eq!(
        response,
        json!({ "request_id": request_id(2), "state": "changed", "agent": agent, "goal": g2 })
    );
    let c = browser_goals(&fixture, agent)
        .first()
        .map(|(lsn, _, _)| *lsn)
        .expect("the change is durable");
    await_entry(&fixture, "G2 met", |lsn, record| {
        lsn > c
            && matches!(
                record,
                Record::LoopDecided { agent: who, decision: LoopDecision::Done, .. } if *who == agent
            )
    });
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let entries = fixture.entries();
    let changes = browser_goals(&fixture, agent);
    assert_eq!(changes.len(), 1, "one browser GoalSet");
    let (_, g2_goal, attribution) = changes.into_iter().next().expect("checked");
    let g1_goal = entries
        .iter()
        .find_map(|(_, e)| match &e.record {
            Record::GoalSet { goal, .. } if goal.id == g1 => Some(goal.clone()),
            _ => None,
        })
        .expect("G1");
    assert_eq!(g2_goal.id, g2);
    assert_eq!(g2_goal.text, NEW_TEXT);
    assert_eq!(g2_goal.predicate, g1_goal.predicate);
    assert_eq!(g2_goal.hop_cap, 3);
    assert_eq!(attribution.replaced_goal, g1);
    assert_eq!(
        attribution.reserved,
        Spend {
            usd: t1_usd,
            ..Spend::default()
        }
    );
    assert_eq!(
        g2_goal.budget,
        GoalBudget {
            usd: Some(Usd::from_micros(ceiling.micros() - t1_usd.micros())),
            tokens: None,
        }
    );
    let session = <[u8; 32]>::from(<sha2::Sha256 as sha2::Digest>::digest(cookie.as_bytes()));
    assert_eq!(attribution.session_digest.bytes(), session);
    assert_eq!(
        attribution.request_digest.bytes(),
        expected_goal_digest(agent, g1, NEW_TEXT)
    );
    let mut second_id = [0_u8; 32];
    second_id[0] = 8;
    assert_eq!(attribution.request_id.bytes(), second_id);

    let (decided_at, decision) = entries
        .iter()
        .find_map(|(lsn, e)| match &e.record {
            Record::LoopDecided {
                agent: who,
                decision,
                ..
            } if *lsn > c && *who == agent => Some((*lsn, decision.clone())),
            _ => None,
        })
        .expect("a decision after C");
    assert!(
        matches!(decision, LoopDecision::Continue { .. }),
        "{decision:?}"
    );

    let (t2, task) = entries
        .iter()
        .find_map(|(_, e)| match &e.record {
            Record::TurnIssued {
                agent: who,
                turn,
                goal,
                task,
                lsn_of_decision,
                ..
            } if *who == agent && *lsn_of_decision == decided_at && *goal == g2 => {
                Some((*turn, task.clone()))
            }
            _ => None,
        })
        .expect("a TurnIssued{goal: G2} for that decision");
    assert!(task.contains(NEW_TEXT), "{task}");
    let tasks: Vec<String> = std::fs::read_dir(capture.path())
        .expect("capture directory")
        .filter_map(Result::ok)
        .filter(|file| file.path().extension().is_some_and(|ext| ext == "task"))
        .map(|file| std::fs::read_to_string(file.path()).expect("captured task"))
        .collect();
    assert_eq!(tasks.len(), 2, "two stub launches");
    assert_eq!(
        tasks.iter().filter(|task| task.contains(NEW_TEXT)).count(),
        1,
        "exactly the G2 launch carried the new text on its stdin"
    );

    let t2_end = entries
        .iter()
        .find_map(|(lsn, e)| match &e.record {
            Record::TurnEnded { turn, .. } if *turn == t2 => Some(*lsn),
            _ => None,
        })
        .expect("T2 ended");
    let verdict = entries
        .iter()
        .find_map(|(lsn, e)| match &e.record {
            Record::PredicateResult {
                goal, pass: true, ..
            } if *goal == g2 => Some(*lsn),
            _ => None,
        })
        .expect("G2's passing verdict");
    let done = entries
        .iter()
        .find_map(|(lsn, e)| match &e.record {
            Record::LoopDecided {
                agent: who,
                decision: LoopDecision::Done,
                ..
            } if *lsn > c && *who == agent => Some(*lsn),
            _ => None,
        })
        .expect("G2 done");
    assert!(
        t2_end < verdict && verdict < done,
        "{t2_end:?} {verdict:?} {done:?}"
    );
    let journal = serde_json::to_string(&entries).expect("journal JSON");
    for secret in [cookie.as_str(), csrf.as_str(), request_id(2).as_str()] {
        assert!(
            !journal.contains(secret),
            "raw browser credential in the journal"
        );
    }

    let daemon = boot(&settings, fake_seams(&settings)).expect("replay boot");
    assert_eq!(daemon.world().agents[&agent].goal, Some(g2));
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    let (head, replayed) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_body(&request_id(2), agent, g1, NEW_TEXT),
    );
    assert_eq!(changed(&head, &replayed), g2);
    assert_eq!(replayed, response);
    assert_eq!(
        fixture.records(),
        before,
        "the index answered; nothing was appended"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Every browser credential is checked before any effect: a wrong Host, a wrong or missing Origin,
/// an unknown cookie, a wrong or missing CSRF and an expired session each get the uniform empty
/// `403`, and the journal does not move. Control: the right credentials apply.
#[test]
fn browser_goal_refuses_wrong_session_host_origin_and_csrf_without_effect() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (agent, goal) = paused_with_goal(&running, &fixture);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = serde_json::to_vec(&goal_body(&request_id(1), agent, goal, NEW_TEXT)).unwrap();
    let host = addr.to_string();
    let request = |host: &str, origin: Option<&str>, cookie: &str, csrf: Option<&str>| {
        let mut head = format!("POST {GOAL_ROUTE} HTTP/1.1\r\nHost: {host}\r\n");
        if let Some(origin) = origin {
            let _ = write!(head, "Origin: {origin}\r\n");
        }
        let _ = write!(head, "Cookie: dudes_floor_session={cookie}\r\n");
        if let Some(csrf) = csrf {
            let _ = write!(head, "X-Dudes-CSRF: {csrf}\r\n");
        }
        let _ = write!(
            head,
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&body);
        bytes
    };
    let origin = format!("http://{host}");
    let unknown = "A".repeat(43);
    let before = fixture.records();
    for (label, bytes) in [
        (
            "wrong Host",
            request("127.0.0.1:1", Some(&origin), &cookie, Some(&csrf)),
        ),
        (
            "wrong Origin",
            request(&host, Some("http://127.0.0.1:1"), &cookie, Some(&csrf)),
        ),
        ("missing Origin", request(&host, None, &cookie, Some(&csrf))),
        (
            "unknown cookie",
            request(&host, Some(&origin), &unknown, Some(&csrf)),
        ),
        (
            "wrong CSRF",
            request(&host, Some(&origin), &cookie, Some(&unknown)),
        ),
        ("missing CSRF", request(&host, Some(&origin), &cookie, None)),
    ] {
        let (head, response) = raw_onboard(addr, &bytes);
        assert!(
            head.starts_with("HTTP/1.1 403 Forbidden"),
            "{label}: {head}"
        );
        assert!(response.is_empty(), "{label}: no envelope");
    }
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let (expired_cookie, expired_csrf) = exchange(addr, &issue(&settings).bootstrap);
    clock.advance(30 * 60 * 1000);
    let (fresh_cookie, fresh_csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, response) = post_goal(
        addr,
        &expired_cookie,
        &expired_csrf,
        &goal_body(&request_id(1), agent, goal, NEW_TEXT),
    );
    empty_refusal(&head, &response, "HTTP/1.1 403 Forbidden");
    assert_eq!(
        fixture.records(),
        before,
        "an expired session appends nothing"
    );
    let _ = (cookie, csrf);

    let (head, response) = post_goal(
        addr,
        &fresh_cookie,
        &fresh_csrf,
        &goal_body(&request_id(1), agent, goal, NEW_TEXT),
    );
    changed(&head, &response);
    assert_eq!(browser_goals(&fixture, agent).len(), 1);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The refusal reasons before the carry-over, in order. An unknown and a retired agent are
/// `agent_absent`; a halted one is `agent_halted`; an older `GoalId` after a local `goal.set`,
/// and any `GoalId` for an agent with no goal, are `stale_goal`; a turn issued and not yet
/// spawned, and a spawned turn, are each `turn_open`. None appends a `GoalSet`. Control: a paused
/// agent with no open turn applies, and its decision is `Hold{Paused}`.
///
/// Mutant: drop the `open_turn` check, and the two open-turn arms append a `GoalSet`.
#[test]
fn browser_goal_refuses_absent_halted_stale_and_open_turn() {
    let program = true_program();
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let host = common::UnconfirmedHost::new(true);
    let daemon = boot(
        &settings,
        seams(
            &settings,
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);

    // The first launch is left unconfirmed, so its turn stays issued and never spawned.
    let issued = common::hire(&running.handle, fixture.state.path());
    let issued_goal = common::set_goal(&running.handle, common::goal_params(issued, &[program]));
    let spawned = common::hire(&running.handle, fixture.state.path());
    let spawned_goal = common::set_goal(&running.handle, common::goal_params(spawned, &[program]));
    fixture.await_record("the spawned turn", |record| {
        matches!(record, Record::TurnSpawned { agent: who, .. } if *who == spawned).then_some(())
    });
    let records = fixture.records();
    assert!(
        records.iter().any(
            |record| matches!(record, Record::TurnIssued { agent: who, .. } if *who == issued)
        ) && !records.iter().any(
            |record| matches!(record, Record::TurnSpawned { agent: who, .. } if *who == issued)
        ),
        "the first turn is issued and not spawned"
    );
    assert_eq!(host.children(), 1, "one child exists: the spawned turn's");

    let (retired, retired_goal) = paused_with_goal(&running, &fixture);
    running
        .handle
        .ask(Ask::Retire(Box::new(RetireParams {
            agent: retired,
            reason: "done".to_owned(),
        })))
        .expect("retire");
    let (halted, halted_goal) = paused_with_goal(&running, &fixture);
    running.handle.ask(Ask::Halt(halted)).expect("local halt");
    let (stale, older) = paused_with_goal(&running, &fixture);
    let newer = common::set_goal(&running.handle, common::goal_params(stale, &[program]));
    let goalless = common::hire(&running.handle, fixture.state.path());
    let (control, control_goal) = paused_with_goal(&running, &fixture);
    let unknown = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("an agent id");

    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    for (k, agent, goal, reason) in [
        (1, unknown, older, "agent_absent"),
        (2, retired, retired_goal, "agent_absent"),
        (3, halted, halted_goal, "agent_halted"),
        (4, stale, older, "stale_goal"),
        (5, goalless, newer, "stale_goal"),
        (6, issued, issued_goal, "turn_open"),
        (7, spawned, spawned_goal, "turn_open"),
    ] {
        let (head, response) = post_goal(
            addr,
            &cookie,
            &csrf,
            &goal_body(&request_id(k), agent, goal, NEW_TEXT),
        );
        refused(&head, &response, reason);
    }
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let (head, response) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_body(&request_id(8), control, control_goal, NEW_TEXT),
    );
    changed(&head, &response);
    let changes = browser_goals(&fixture, control);
    assert_eq!(changes.len(), 1, "control: a paused agent applies");
    let c = changes[0].0;
    assert!(
        after(&fixture, c).iter().any(|(_, e)| matches!(
            &e.record,
            Record::LoopDecided {
                agent: who,
                decision: LoopDecision::Hold {
                    reason: dudes_proto::HoldReason::Paused
                },
                ..
            } if *who == control
        )),
        "the change decides Hold{{Paused}}"
    );
    for agent in [issued, spawned, retired, halted, stale, goalless] {
        assert!(browser_goals(&fixture, agent).is_empty(), "{agent}");
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Invariant 4 across a chain of changes. G1 has a hop cap of 3; after each turn's verdict the
/// agent is held by a local pause, a browser change applies, and a local resume issues the next
/// turn. The third change is `goal_exhausted` with no `GoalSet`, and exactly three Managed turns
/// were issued across the chain. A dollar arm is refused at a zero dollar remainder. Control: a
/// local `goal.set` funds more, and the agent issues a fourth turn.
///
/// Mutant: copy G1's hop cap instead of the remainder, and the third change applies.
#[test]
fn browser_goal_chain_never_exceeds_the_first_goal() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let (parts, native_host) = native_fixture_seams(&fixture, fake_seams(&settings));
    let daemon = boot(&settings, parts).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let agent = common::hire(&running.handle, fixture.state.path());
    running.handle.ask(Ask::Pause(agent)).expect("local pause");
    let mut goal = common::set_goal(
        &running.handle,
        GoalSetParams {
            hop_cap: 3,
            ..common::goal_params(agent, &[true_program()])
        },
    );
    let issued = |fixture: &Fixture| {
        fixture
            .records()
            .iter()
            .filter(|record| {
                matches!(record, Record::TurnIssued { agent: who, mode: dudes_types::Mode::Managed, .. } if *who == agent)
            })
            .count()
    };
    for k in 0..3 {
        running
            .handle
            .ask(Ask::Resume(agent))
            .expect("local resume");
        let turn = issued_goal_turn(&fixture, goal);
        assert_eq!(issued(&fixture), k + 1, "the resume issued one turn");
        running.handle.ask(Ask::Pause(agent)).expect("local pause");
        native.complete(turn, Some(dollars(Usd::from_cents(1))));
        let judged = goal;
        fixture.await_record("the verdict", |record| {
            matches!(record, Record::PredicateResult { goal, .. } if *goal == judged).then_some(())
        });
        let (head, response) = post_goal(
            addr,
            &cookie,
            &csrf,
            &goal_body(&request_id(k + 1), agent, goal, NEW_TEXT),
        );
        if k < 2 {
            goal = changed(&head, &response);
            let (_, new_goal, _) = browser_goals(&fixture, agent).pop().expect("the change");
            assert_eq!(new_goal.hop_cap, 2 - u32::try_from(k).unwrap());
        } else {
            refused(&head, &response, "goal_exhausted");
            assert_eq!(browser_goals(&fixture, agent).len(), 2, "no third GoalSet");
        }
    }
    assert_eq!(browser_goals(&fixture, agent).len(), 2);
    assert_eq!(issued(&fixture), 3, "the chain issued G1's three turns");

    let (dollars, spent_goal) = spent_dollar_goal(&native);
    let (head, response) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_body(&request_id(4), dollars, spent_goal, NEW_TEXT),
    );
    refused(&head, &response, "goal_exhausted");
    assert!(browser_goals(&fixture, dollars).is_empty());

    let control_goal = common::set_goal(
        &running.handle,
        common::goal_params(agent, &[true_program()]),
    );
    running
        .handle
        .ask(Ask::Resume(agent))
        .expect("local resume");
    issued_goal_turn(&fixture, control_goal);
    assert_eq!(issued(&fixture), 4, "control: the local goal issued a turn");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

fn spent_dollar_goal(native: &NativeTurnFixture<'_>) -> (AgentId, GoalId) {
    let (running, fixture) = (native.running, native.fixture);
    let dollars = common::hire(&running.handle, fixture.state.path());
    running
        .handle
        .ask(Ask::Pause(dollars))
        .expect("local pause");
    let spent_goal = common::set_goal(
        &running.handle,
        common::goal_params_with_budget(dollars, &[true_program()], Usd::from_cents(10)),
    );
    running
        .handle
        .ask(Ask::Resume(dollars))
        .expect("local resume");
    let turn = issued_goal_turn(fixture, spent_goal);
    running
        .handle
        .ask(Ask::Pause(dollars))
        .expect("local pause");
    native.complete(
        turn,
        Some(Usage {
            usd: Some(Usd::from_cents(10)),
            ..Usage::ZERO
        }),
    );
    fixture.await_record("the dollar arm's verdict", |record| {
        matches!(record, Record::PredicateResult { goal, .. } if *goal == spent_goal).then_some(())
    });
    (dollars, spent_goal)
}

/// One step of [`GatedRunner`]'s script.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// Wait for [`GatedRunner::release`], then pass.
    Held,
    /// The run fails to start: no verdict.
    Unrun,
    /// The exit path swept it: no verdict.
    Swept,
    /// The runner panics inside the predicate thread.
    Panics,
}

/// What [`GatedRunner`] reports.
#[derive(Debug, PartialEq, Eq)]
enum RunEvent {
    Started,
    Returning,
}

/// A predicate runner that answers from a script of [`Step`]s, then passes, and reports each run's
/// start and return.
struct GatedRunner {
    script: Mutex<std::collections::VecDeque<Step>>,
    released: (Mutex<bool>, std::sync::Condvar),
    events: Mutex<mpsc::Sender<RunEvent>>,
    runs: std::sync::atomic::AtomicUsize,
}

impl GatedRunner {
    fn new(script: impl IntoIterator<Item = Step>) -> (Arc<Self>, mpsc::Receiver<RunEvent>) {
        let (tx, rx) = mpsc::channel();
        (
            Arc::new(Self {
                script: Mutex::new(script.into_iter().collect()),
                released: (Mutex::new(false), std::sync::Condvar::new()),
                events: Mutex::new(tx),
                runs: std::sync::atomic::AtomicUsize::new(0),
            }),
            rx,
        )
    }

    fn release(&self) {
        *self
            .released
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        self.released.1.notify_all();
    }

    fn report(&self, event: RunEvent) {
        let _ = self
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(event);
    }
}

impl duded::predicate::PredicateRunner for GatedRunner {
    fn run(
        &self,
        _spec: &LaunchSpec,
        _timeout: Duration,
    ) -> std::io::Result<duded::predicate::PredicateOutcome> {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let step = self
            .script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        self.report(RunEvent::Started);
        if matches!(step, Some(Step::Held)) {
            let mut released = self
                .released
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            while !*released {
                released = self
                    .released
                    .1
                    .wait(released)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        self.report(RunEvent::Returning);
        match step {
            Some(Step::Unrun) => Err(std::io::Error::other("the predicate could not start")),
            Some(Step::Swept) => Ok(duded::predicate::PredicateOutcome::Swept),
            Some(Step::Panics) => panic!("an injected predicate runner panic"),
            Some(Step::Held) | None => Ok(common::passes()),
        }
    }

    fn sweep(&self) {
        self.release();
    }
}

/// One daemon over a fake host and a [`GatedRunner`], with one agent whose T1 has ended and
/// handed G1's predicate to the runner. Control is on, so the verdict arms can also hold the
/// writer with a pause on a second agent.
struct VerdictArm {
    fixture: Fixture,
    running: common::Running,
    runner: Arc<GatedRunner>,
    events: mpsc::Receiver<RunEvent>,
    pause: duded::runtime::OnboardingPause,
    addr: SocketAddr,
    cookie: String,
    csrf: String,
    agent: AgentId,
    goal: GoalId,
}

impl VerdictArm {
    fn start(script: impl IntoIterator<Item = Step>, spawn_failure: bool) -> Self {
        let fixture = Fixture::new();
        let settings = goal_settings(&fixture, true);
        let (runner, events) = GatedRunner::new(script);
        let (parts, native_host) = native_fixture_seams(
            &fixture,
            seams(
                &settings,
                SharedHost::new(),
                Arc::clone(&runner),
                Arc::new(TestClock::new()),
                Witness::default(),
            ),
        );
        let daemon = boot(&settings, parts).expect("boot Floor");
        if spawn_failure {
            daemon.predicate_spawn_failure().arm();
        }
        let pause = daemon.onboarding_pause();
        let addr = daemon.floor_addr().expect("Floor address");
        let running = common::start(daemon);
        let agent = common::hire(&running.handle, fixture.state.path());
        let goal = common::set_goal(
            &running.handle,
            common::goal_params(agent, &[true_program()]),
        );
        let turn = issued_goal_turn(&fixture, goal);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        NativeTurnFixture {
            running: &running,
            fixture: &fixture,
            host: &native_host,
        }
        .complete(turn, Some(dollars(Usd::from_cents(1))));
        fixture.await_record("T1 ended", |record| {
            matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn).then_some(())
        });
        Self {
            fixture,
            running,
            runner,
            events,
            pause,
            addr,
            cookie,
            csrf,
            agent,
            goal,
        }
    }

    fn expect(&self, event: RunEvent) {
        assert_eq!(
            self.events.recv_timeout(Duration::from_secs(5)),
            Ok(event),
            "the runner's report"
        );
    }

    fn body(&self, k: usize, text: &str) -> Value {
        goal_body(&request_id(k), self.agent, self.goal, text)
    }

    fn post(&self, body: &Value) -> (String, Value) {
        post_goal(self.addr, &self.cookie, &self.csrf, body)
    }

    fn verdicts(&self) -> usize {
        self.fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::PredicateResult { goal, .. } if *goal == self.goal))
            .count()
    }

    fn stop(mut self) {
        self.runner.release();
        assert_eq!(
            self.running.stop(ExitCause::ShutdownRpc),
            ExitCause::ShutdownRpc
        );
    }
}

/// A change waits for the replaced goal's verdict, and the mark that says a run is
/// outstanding is set and cleared only on the writer thread. Each arm runs after T1 has ended.
///
/// - **Held verdict:** while G1's predicate is held, a change is `turn_open` with no `GoalSet`.
/// - **Queued behind the verdict:** with the writer held, one change is queued, then the runner
///   returns G1's verdict, then a second change is queued. The first is `turn_open`; the second
///   applies, and G1's `PredicateResult` precedes its `GoalSet`.
/// - **No verdict:** a run that could not start, and a swept run, clear the mark; a change then
///   applies, and no verdict was journaled.
/// - **Panic:** a runner that panics clears the mark through the thread's guard.
/// - **Spawn failure:** a predicate thread that never starts clears the mark in the error arm.
///
/// Mutants, each red here: drop the predicate-run check (the held arm appends a `GoalSet`); clear
/// the mark on the runner's thread after posting the verdict (the queued arm's first change
/// applies before G1's `PredicateResult`); clear the mark only on a verdict (the no-verdict and
/// panic arms stay refused); leave the spawn-error arm alone (the spawn-failure arm stays
/// refused).
#[test]
fn browser_goal_waits_for_the_replaced_goals_verdict() {
    let arm = VerdictArm::start([Step::Held], false);
    arm.expect(RunEvent::Started);
    let (head, response) = arm.post(&arm.body(1, NEW_TEXT));
    refused(&head, &response, "turn_open");
    assert!(browser_goals(&arm.fixture, arm.agent).is_empty());
    arm.runner.release();
    arm.expect(RunEvent::Returning);
    apply_goal_when_admitted(arm.addr, &arm.cookie, &arm.csrf, &arm.body(1, NEW_TEXT));
    assert_eq!(arm.verdicts(), 1);
    arm.stop();

    let arm = VerdictArm::start([Step::Held], false);
    arm.expect(RunEvent::Started);
    let blocker = common::hire(&arm.running.handle, arm.fixture.state.path());
    arm.pause.arm(OnboardingPausePoint::BeforeControlPermit);
    let blocking = {
        let (addr, cookie, csrf) = (arm.addr, arm.cookie.clone(), arm.csrf.clone());
        std::thread::spawn(move || {
            post_control(
                addr,
                PAUSE,
                &cookie,
                &csrf,
                &control_body(&request_id(9), blocker, None),
            )
        })
    };
    assert!(
        arm.pause.wait_reached(Duration::from_secs(5)),
        "the writer is held"
    );
    let queue = |body: Value| {
        let (sent_tx, sent_rx) = mpsc::channel();
        let (addr, cookie, csrf) = (arm.addr, arm.cookie.clone(), arm.csrf.clone());
        let request = std::thread::spawn(move || {
            post_bytes(
                addr,
                GOAL_ROUTE,
                &cookie,
                &csrf,
                &serde_json::to_vec(&body).expect("goal JSON"),
                Some(sent_tx),
            )
        });
        sent_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("the request is sent");
        // The connection thread admits it and queues its job behind the held writer.
        std::thread::sleep(Duration::from_millis(200));
        request
    };
    let ahead = queue(arm.body(1, "ahead of the verdict"));
    arm.runner.release();
    arm.expect(RunEvent::Returning);
    // Let the runner's thread post the verdict before the second change is queued.
    std::thread::sleep(Duration::from_millis(200));
    let behind = queue(arm.body(2, "behind the verdict"));
    arm.pause.release();
    let (head, response) = blocking.join().expect("the blocking pause");
    applied(&head, &response, "paused");
    let (head, response) = ahead.join().expect("the change ahead");
    refused(&head, &response, "turn_open");
    let (head, response) = behind.join().expect("the change behind");
    changed(&head, &response);
    let changes = browser_goals(&arm.fixture, arm.agent);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].1.text, "behind the verdict");
    let verdict = arm
        .fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, e)| match e.record {
            Record::PredicateResult { goal, .. } if goal == arm.goal => Some(lsn),
            _ => None,
        })
        .expect("G1's verdict");
    assert!(verdict < changes[0].0, "the verdict precedes the change");
    arm.stop();

    for step in [Step::Unrun, Step::Swept] {
        let arm = VerdictArm::start([step], false);
        arm.expect(RunEvent::Started);
        arm.expect(RunEvent::Returning);
        apply_goal_when_admitted(arm.addr, &arm.cookie, &arm.csrf, &arm.body(1, NEW_TEXT));
        assert_eq!(arm.verdicts(), 0, "{step:?}: no verdict was observed");
        arm.stop();
    }

    let arm = VerdictArm::start([Step::Panics], false);
    arm.expect(RunEvent::Started);
    arm.expect(RunEvent::Returning);
    apply_goal_when_admitted(arm.addr, &arm.cookie, &arm.csrf, &arm.body(1, NEW_TEXT));
    assert_eq!(arm.verdicts(), 0);
    arm.stop();

    let arm = VerdictArm::start([], true);
    apply_goal_when_admitted(arm.addr, &arm.cookie, &arm.csrf, &arm.body(1, NEW_TEXT));
    assert_eq!(
        arm.runner.runs.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "G1's predicate thread never started"
    );
    assert_eq!(arm.verdicts(), 0);
    arm.stop();
}

/// Every applied body is single use. Resent under a new identity it is `stale_goal`; resent under
/// its own it gets its original `200`. No new record either way.
#[test]
fn browser_goal_replayed_body_has_no_effect() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (agent, goal) = paused_with_goal(&running, &fixture);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = goal_body(&request_id(1), agent, goal, NEW_TEXT);
    let (head, original) = post_goal(addr, &cookie, &csrf, &body);
    changed(&head, &original);
    let applied_records = fixture.records();

    let mut replayed = body.clone();
    replayed["request_id"] = json!(request_id(2));
    let (head, response) = post_goal(addr, &cookie, &csrf, &replayed);
    refused(&head, &response, "stale_goal");
    let (head, response) = post_goal(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(response, original, "its own identity gets its original 200");
    assert_eq!(fixture.records(), applied_records, "zero new records");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The journal-derived index answers a same-key retry with its original result — after a
/// later local `goal.set`, and from a fresh session after a restart — with no new record. The same
/// key with a changed payload is an empty `409`.
#[test]
fn browser_goal_same_key_retry_returns_original_and_changed_payload_conflicts() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (agent, goal) = paused_with_goal(&running, &fixture);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = goal_body(&request_id(1), agent, goal, NEW_TEXT);
    let (head, original) = post_goal(addr, &cookie, &csrf, &body);
    changed(&head, &original);
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[true_program()]),
    );
    let later = fixture.records();

    let (head, response) = post_goal(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(response, original, "after a later local goal.set");
    let mut changed_text = body.clone();
    changed_text["text"] = json!("another text");
    let (head, response) = post_goal(addr, &cookie, &csrf, &changed_text);
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    assert_eq!(fixture.records(), later, "no retry appended anything");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let daemon = boot(&settings, fake_seams(&settings)).expect("restart");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    let (head, response) = post_goal(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(response, original, "from a fresh session after a restart");
    let (head, response) = post_goal(addr, &cookie, &csrf, &changed_text);
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    assert_eq!(fixture.records(), before);
    assert_eq!(browser_goals(&fixture, agent).len(), 1);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// While a request is in flight: with the owner past its world check and carry-over and before
/// its append permit, a same-key follower gets `202 pending` and a changed payload an empty
/// `409`, both without the writer. Then the owner's `200`. Two keys sent from one page view yield
/// one record and one `stale_goal`.
#[test]
fn browser_goal_concurrent_same_key_is_pending_and_two_keys_yield_one_record() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (agent, goal) = paused_with_goal(&running, &fixture);
    let (second, second_goal) = paused_with_goal(&running, &fixture);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = goal_body(&request_id(1), agent, goal, NEW_TEXT);

    pause.arm(OnboardingPausePoint::BeforeGoalChangePermit);
    let owner = {
        let (cookie, csrf, body) = (cookie.clone(), csrf.clone(), body.clone());
        std::thread::spawn(move || post_goal(addr, &cookie, &csrf, &body))
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the owner is in flight"
    );
    let at_once = |body: Value| {
        let (tx, rx) = mpsc::channel();
        let (cookie, csrf) = (cookie.clone(), csrf.clone());
        std::thread::spawn(move || {
            let _ = tx.send(post_goal(addr, &cookie, &csrf, &body));
        });
        rx.recv_timeout(Duration::from_secs(3)).ok()
    };
    let follower = at_once(body.clone());
    let mut changed_text = body.clone();
    changed_text["text"] = json!("another text");
    let conflicting = at_once(changed_text);
    pause.release();
    let (head, pending) = follower.expect("a same-key follower answers without the writer");
    assert!(head.starts_with("HTTP/1.1 202 Accepted"), "{head}");
    assert_eq!(
        pending,
        json!({ "request_id": request_id(1), "state": "pending" })
    );
    let (head, response) = conflicting.expect("a changed payload answers without the writer");
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    let (head, response) = owner.join().expect("owner");
    changed(&head, &response);
    assert_eq!(browser_goals(&fixture, agent).len(), 1);

    let view = goal_body(&request_id(2), second, second_goal, NEW_TEXT);
    let mut twin = view.clone();
    twin["request_id"] = json!(request_id(3));
    let (head, response) = post_goal(addr, &cookie, &csrf, &view);
    changed(&head, &response);
    let (head, response) = post_goal(addr, &cookie, &csrf, &twin);
    refused(&head, &response, "stale_goal");
    assert_eq!(browser_goals(&fixture, second).len(), 1, "one record");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Consequence of the single writer: a local `goal.set` that reaches the writer first is never overwritten. One
/// that lands between the page's view and the request makes it `stale_goal`; and a request whose
/// append failed before its fsync, then a local `goal.set`, then a same-key retry, is refused.
#[test]
fn browser_goal_local_winner_is_never_overwritten() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let append_failure = daemon.append_failure();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (moved, seen) = paused_with_goal(&running, &fixture);
    let (crashed, crashed_goal) = paused_with_goal(&running, &fixture);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);

    let local = common::set_goal(
        &running.handle,
        common::goal_params(moved, &[true_program()]),
    );
    let before = fixture.records();
    let (head, response) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_body(&request_id(1), moved, seen, NEW_TEXT),
    );
    refused(&head, &response, "stale_goal");
    assert_eq!(fixture.records(), before, "the local goal stands");
    assert_eq!(folded(&fixture).agents[&moved].goal, Some(local));

    append_failure.fail_nth(NonZeroUsize::new(1).unwrap());
    let body = goal_body(&request_id(2), crashed, crashed_goal, NEW_TEXT);
    let (head, response) = post_goal(addr, &cookie, &csrf, &body);
    empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
    assert!(browser_goals(&fixture, crashed).is_empty());
    let local = common::set_goal(
        &running.handle,
        common::goal_params(crashed, &[true_program()]),
    );
    let (head, response) = post_goal(addr, &cookie, &csrf, &body);
    refused(&head, &response, "stale_goal");
    assert!(browser_goals(&fixture, crashed).is_empty());
    assert_eq!(folded(&fixture).agents[&crashed].goal, Some(local));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "three restart cuts share one scenario and one assertion shape"
)]
fn browser_goal_restart_cut_points_replay_one_record() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Cut {
        GoalSetFsync,
        GoalSetFold,
        TurnIssuedFsync,
    }
    for cut in [Cut::GoalSetFsync, Cut::GoalSetFold, Cut::TurnIssuedFsync] {
        let fixture = Fixture::new();
        let settings = goal_settings(&fixture, true);
        let run = || {
            let (parts, host) = native_fixture_seams(
                &fixture,
                seams(
                    &settings,
                    SharedHost::new(),
                    Arc::new(ScriptedRunner::scripted([common::passes()])),
                    Arc::new(TestClock::new()),
                    Witness::default(),
                ),
            );
            (boot(&settings, parts).expect("boot Floor"), host)
        };
        let (daemon, host) = run();
        let failure = daemon.onboarding_failure();
        let append_failure = daemon.append_failure();
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let native = NativeTurnFixture {
            running: &running,
            fixture: &fixture,
            host: &host,
        };
        let (agent, goal) = if cut == Cut::TurnIssuedFsync {
            // A `Done` agent: the change's own decision issues G2's turn.
            let agent = common::hire(&running.handle, fixture.state.path());
            let goal = common::set_goal(
                &running.handle,
                common::goal_params(agent, &[true_program()]),
            );
            let turn = issued_goal_turn(&fixture, goal);
            native.complete(turn, Some(dollars(Usd::from_cents(1))));
            fixture.await_record("G1 met", |record| {
                matches!(record, Record::LoopDecided { agent: who, decision: LoopDecision::Done, .. } if *who == agent)
                    .then_some(())
            });
            (agent, goal)
        } else {
            paused_with_goal(&running, &fixture)
        };
        let (other, other_goal) = paused_with_goal(&running, &fixture);
        let spawned_before = host.spawned().len();
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let body = goal_body(&request_id(1), agent, goal, NEW_TEXT);
        match cut {
            Cut::GoalSetFsync => failure.arm(OnboardingFailurePoint::GoalChange),
            Cut::GoalSetFold => append_failure.fail_nth(NonZeroUsize::new(2).unwrap()),
            Cut::TurnIssuedFsync => failure.arm(OnboardingFailurePoint::TurnIssued),
        }
        let (head, response) = post_goal(addr, &cookie, &csrf, &body);
        if cut == Cut::GoalSetFsync {
            empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
            let (head, pending) = post_goal(addr, &cookie, &csrf, &body);
            assert!(head.starts_with("HTTP/1.1 202 Accepted"), "{head}");
            assert_eq!(
                pending["state"], "pending",
                "an uncertain append keeps its key"
            );
            let fresh = goal_body(&request_id(2), other, other_goal, NEW_TEXT);
            let (head, response) = post_goal(addr, &cookie, &csrf, &fresh);
            empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
        } else {
            changed(&head, &response);
        }
        if cut == Cut::TurnIssuedFsync {
            let goal = browser_goals(&fixture, agent).last().unwrap().1.id;
            nth_turn(&fixture, goal, 0);
            running.handle.doctor().unwrap();
        }
        assert_eq!(
            host.spawned().len(),
            spawned_before,
            "{cut:?}: the cut is before any Spawn"
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
        let (c, g2_goal, _) = browser_goals(&fixture, agent)
            .pop()
            .expect("the GoalSet is durable");
        match cut {
            Cut::GoalSetFsync | Cut::GoalSetFold => assert!(
                !after(&fixture, c)
                    .iter()
                    .any(|(_, e)| matches!(&e.record, Record::LoopDecided { agent: who, .. } if *who == agent)),
                "{cut:?}: no decision after the change yet"
            ),
            Cut::TurnIssuedFsync => assert!(
                after(&fixture, c).iter().any(|(_, e)| matches!(&e.record, Record::TurnIssued { goal, .. } if *goal == g2_goal.id)),
                "{cut:?}: G2's turn is durable"
            ),
        }

        let (daemon, restarted_host) = run();
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        if cut == Cut::TurnIssuedFsync {
            recover_owed_browser_turn(&settings, &fixture, &running, agent, g2_goal.id);
            issued_goal_turn(&fixture, g2_goal.id);
            assert_eq!(
                restarted_host.spawned().len(),
                1,
                "G2's exact owed turn re-issued once"
            );
        }
        let (head, response) = post_goal(addr, &cookie, &csrf, &body);
        assert_eq!(changed(&head, &response), g2_goal.id, "{cut:?}");
        let records = fixture.records();
        assert_eq!(browser_goals(&fixture, agent).len(), 1, "{cut:?}");
        assert!(browser_goals(&fixture, other).is_empty(), "{cut:?}");
        assert!(
            after(&fixture, c).iter().any(|(_, e)| matches!(&e.record, Record::LoopDecided { agent: who, .. } if *who == agent)),
            "{cut:?}: replay decided on G2"
        );
        let g2_turns = records
            .iter()
            .filter(
                |record| matches!(record, Record::TurnIssued { goal, .. } if *goal == g2_goal.id),
            )
            .count();
        if cut == Cut::TurnIssuedFsync {
            assert_eq!(g2_turns, 1, "{cut:?}: G2's turn re-issued exactly once");
            assert_eq!(
                host.spawned().len() + restarted_host.spawned().len(),
                spawned_before + 1,
                "{cut:?}: one Spawn"
            );
        } else {
            assert_eq!(g2_turns, 0, "{cut:?}: the paused agent issues nothing");
            assert_eq!(host.spawned().len(), spawned_before, "{cut:?}");
        }
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

/// A correctly identified live session passes the world check and the carry-over, is revoked
/// before its append permit, and gets the uniform empty `403` with no `GoalSet`, `LoopDecided` or
/// `TurnIssued`. In the opposite order, revocation waits for the held permit's fsync and the
/// request gets its `200`.
///
/// Mutants: the writer's permit match omits a `GoalSet` carrying `browser_goal`; `permit` returns
/// without consulting the registry. Deleting only the `revoked` flag check is not a mutant:
/// revocation also removes the session from the registry.
#[test]
fn browser_goal_revocation_before_the_permit_is_forbidden() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let authorizer = daemon.floor_session_authorizer().expect("Floor sessions");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (agent, goal) = paused_with_goal(&running, &fixture);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = goal_body(&request_id(1), agent, goal, NEW_TEXT);
    let before = fixture.records();

    pause.arm(OnboardingPausePoint::BeforeGoalChangePermit);
    let losing = {
        let (cookie, csrf, body) = (cookie.clone(), csrf.clone(), body.clone());
        std::thread::spawn(move || post_goal(addr, &cookie, &csrf, &body))
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "admitted, world-checked, before the permit"
    );
    assert!(
        authorizer.revoke_for_test(&cookie),
        "the live session is revoked"
    );
    pause.release();
    let (head, response) = losing.join().expect("revoked request");
    empty_refusal(&head, &response, "HTTP/1.1 403 Forbidden");
    let after_revoke = fixture.records();
    assert!(
        !after_revoke[before.len()..].iter().any(|record| matches!(
            record,
            Record::GoalSet { .. } | Record::LoopDecided { .. } | Record::TurnIssued { .. }
        )),
        "{:?}",
        &after_revoke[before.len()..]
    );

    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    pause.arm(OnboardingPausePoint::AfterGoalChangePermit);
    let owner = {
        let (cookie, csrf) = (cookie.clone(), csrf.clone());
        std::thread::spawn(move || post_goal(addr, &cookie, &csrf, &body))
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the append holds its permit"
    );
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let revoke = authorizer.clone();
    let revoker = std::thread::spawn(move || {
        started_tx.send(()).expect("started signal");
        done_tx
            .send(revoke.revoke_for_test(&cookie))
            .expect("revoke result");
    });
    started_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("revoker started");
    assert!(
        done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "revocation waits while the append permit is held"
    );
    pause.release();
    assert!(
        done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("revocation completed")
    );
    revoker.join().expect("revoker thread");
    let (head, response) = owner.join().expect("winning request");
    changed(&head, &response);
    let _ = csrf;
    assert_eq!(browser_goals(&fixture, agent).len(), 1);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// No clock read happens under the registry lock: session A holds its change's append
/// permit — and so the registry lock — while session B's change waits for admission and the test
/// clock crosses the absolute expiry. A's held permit wins; B gets the uniform empty `403` and
/// appends nothing; and no clock read happens while A holds the lock.
#[test]
fn browser_goal_expiry_waiting_on_registry_lock_is_forbidden() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let clock = Arc::new(ProbeClock {
        inner: TestClock::ticking(1_800_000_000_000, 0),
        armed: Mutex::new(None),
    });
    let daemon = boot(
        &settings,
        Seams {
            host: SharedHost::new(),
            pty_host: Box::new(duded::ports::ClosedPtyHost),
            predicates: Arc::new(ScriptedRunner::default()),
            clock: Arc::clone(&clock),
            rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
            resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
            adapters: [Box::new(StubAdapter) as Box<dyn Adapter>]
                .into_iter()
                .collect(),
            children: ChildEnds::new(),
            witness: Witness::default(),
        },
    )
    .expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (a, a_goal) = paused_with_goal(&running, &fixture);
    let (b, b_goal) = paused_with_goal(&running, &fixture);
    let (a_cookie, a_csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (b_cookie, b_csrf) = exchange(addr, &issue(&settings).bootstrap);

    pause.arm(OnboardingPausePoint::AfterGoalChangePermit);
    let holder = std::thread::spawn(move || {
        post_goal(
            addr,
            &a_cookie,
            &a_csrf,
            &goal_body(&request_id(1), a, a_goal, NEW_TEXT),
        )
    });
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "A holds its append permit"
    );
    let probe = clock.arm();
    let (sent_tx, sent_rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let body = serde_json::to_vec(&goal_body(&request_id(2), b, b_goal, NEW_TEXT)).unwrap();
        post_bytes(addr, GOAL_ROUTE, &b_cookie, &b_csrf, &body, Some(sent_tx))
    });
    sent_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("B is sent while A holds the lock");
    let sampled_while_held = probe.recv_timeout(Duration::from_millis(500)).is_ok();
    clock.inner.advance(8 * 60 * 60 * 1000);
    pause.release();
    let (head, winner) = holder.join().expect("A");
    changed(&head, &winner);
    let (head, response) = waiter.join().expect("B");
    assert!(
        probe.recv_timeout(Duration::from_secs(2)).is_ok() || sampled_while_held,
        "positive control: the clock was read once armed"
    );
    assert!(
        !sampled_while_held,
        "session validity was sampled while another session held the registry lock"
    );
    empty_refusal(&head, &response, "HTTP/1.1 403 Forbidden");
    assert_eq!(browser_goals(&fixture, a).len(), 1);
    assert!(
        browser_goals(&fixture, b).is_empty(),
        "the expired waiter appended nothing"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// An uncertain local write fences the daemon: a durable change's same-key retry is still served
/// from the index, with its original result and a changed payload's conflict, while a fresh
/// request gets an empty `503`, appends nothing, and releases its key.
#[test]
fn browser_goal_uncertain_local_write_fences_fresh_requests_and_serves_durable_retries() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let failure = daemon.onboarding_failure();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (first, first_goal) = paused_with_goal(&running, &fixture);
    let (second, second_goal) = paused_with_goal(&running, &fixture);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let durable = goal_body(&request_id(1), first, first_goal, NEW_TEXT);
    let (head, original) = post_goal(addr, &cookie, &csrf, &durable);
    changed(&head, &original);

    failure.arm(OnboardingFailurePoint::LocalGoal);
    let local = running
        .handle
        .ask(Ask::SetGoal(Box::new(common::goal_params(
            first,
            &[true_program()],
        ))));
    assert!(
        local.is_err(),
        "the local goal is durable and then fails its fold"
    );
    let fenced = fixture.records();

    for attempt in 0..2 {
        let (head, retry) = post_goal(addr, &cookie, &csrf, &durable);
        assert!(
            head.starts_with("HTTP/1.1 200 OK"),
            "retry {attempt} while fenced: {head}"
        );
        assert_eq!(retry, original);
    }
    let mut changed_text = durable.clone();
    changed_text["text"] = json!("another text");
    let (head, response) = post_goal(addr, &cookie, &csrf, &changed_text);
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    let fresh = goal_body(&request_id(2), second, second_goal, NEW_TEXT);
    for _ in 0..2 {
        let (head, response) = post_goal(addr, &cookie, &csrf, &fresh);
        empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
    }
    assert_eq!(fixture.records(), fenced, "nothing appended while fenced");
    assert!(browser_goals(&fixture, second).is_empty());
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The request bounds refuse before any effect: 2,049 bytes after trim, nothing after trim, U+0000, a
/// lone surrogate, an unknown, a duplicate, a nested and a missing member, a noncanonical request
/// identity, agent or goal, and a 16,385-byte body (`413`, before a byte is read). Control: 2,048
/// bytes of U+0001, which `JSON.stringify` escapes to a 12,431-byte body, applies.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "every request bound is one row of a single table beside its control"
)]
fn browser_goal_bounds_and_json_shape_refuse_before_effect() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (agent, goal) = paused_with_goal(&running, &fixture);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let id = request_id(1);
    let alias = agent.to_string().replacen('0', "O", 1);
    assert_ne!(alias, agent.to_string(), "the alias spelling differs");
    let goal_alias = goal.to_string().replacen('0', "O", 1);
    assert_ne!(goal_alias, goal.to_string(), "the alias spelling differs");
    let over = "x".repeat(2049);
    let refusals: Vec<(&str, String)> = vec![
        (
            "2,049 bytes after trim",
            format!(
                r#"{{"request_id":"{id}","agent":"{agent}","goal":"{goal}","text":" {over} "}}"#
            ),
        ),
        (
            "nothing after trim",
            format!(
                r#"{{"request_id":"{id}","agent":"{agent}","goal":"{goal}","text":" \t\r\n"}}"#
            ),
        ),
        (
            "U+0000",
            format!(
                r#"{{"request_id":"{id}","agent":"{agent}","goal":"{goal}","text":"a\u0000b"}}"#
            ),
        ),
        (
            "a lone surrogate",
            format!(
                r#"{{"request_id":"{id}","agent":"{agent}","goal":"{goal}","text":"a\ud800b"}}"#
            ),
        ),
        (
            "unknown member",
            format!(
                r#"{{"request_id":"{id}","agent":"{agent}","goal":"{goal}","text":"t","predicate":1}}"#
            ),
        ),
        (
            "duplicate member",
            format!(
                r#"{{"request_id":"{id}","agent":"{agent}","goal":"{goal}","text":"t","text":"u"}}"#
            ),
        ),
        (
            "nested member",
            format!(
                r#"{{"request_id":"{id}","agent":"{agent}","goal":"{goal}","text":{{"t":1}}}}"#
            ),
        ),
        (
            "missing member",
            format!(r#"{{"request_id":"{id}","agent":"{agent}","text":"t"}}"#),
        ),
        (
            "noncanonical request_id",
            format!(
                r#"{{"request_id":"{}B","agent":"{agent}","goal":"{goal}","text":"t"}}"#,
                "A".repeat(42)
            ),
        ),
        (
            "alias-letter agent",
            format!(r#"{{"request_id":"{id}","agent":"{alias}","goal":"{goal}","text":"t"}}"#),
        ),
        (
            "alias-letter goal",
            format!(
                r#"{{"request_id":"{id}","agent":"{agent}","goal":"{goal_alias}","text":"t"}}"#
            ),
        ),
        ("an array", format!(r#"["{id}","{agent}","{goal}","t"]"#)),
    ];
    let before = fixture.records();
    for (label, body) in &refusals {
        let (head, response) = post_bytes(addr, GOAL_ROUTE, &cookie, &csrf, body.as_bytes(), None);
        assert!(
            head.starts_with("HTTP/1.1 400 Bad Request"),
            "{label}: {head}"
        );
        assert!(response.is_null(), "{label}: {response}");
    }
    let host = addr.to_string();
    let oversize = format!(
        "POST {GOAL_ROUTE} HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: dudes_floor_session={cookie}\r\nX-Dudes-CSRF: {csrf}\r\nContent-Type: application/json\r\nContent-Length: 16385\r\n\r\n"
    );
    let (head, body) = raw_onboard(addr, oversize.as_bytes());
    assert!(
        head.starts_with("HTTP/1.1 413"),
        "refused before a byte is read: {head}"
    );
    assert!(body.is_empty());
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let widest = serde_json::to_vec(&goal_body(
        &request_id(2),
        agent,
        goal,
        &"\u{1}".repeat(2048),
    ))
    .unwrap();
    assert_eq!(widest.len(), 12_431);
    let (head, response) = post_bytes(addr, GOAL_ROUTE, &cookie, &csrf, &widest, None);
    changed(&head, &response);
    let (_, new_goal, _) = browser_goals(&fixture, agent).pop().expect("applied");
    assert_eq!(new_goal.text, "\u{1}".repeat(2048));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// With goal change off — the default, with or without pause and resume — the route does not
/// exist: its response is byte for byte an unrouted POST's, and no snapshot carries the `goal`
/// sibling. Control: goal change on routes it and sends the sibling. And the operator's switch
/// loads with no budget, and is refused at load without `[floor.control]` enabled.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "three switch settings and every load refusal share one table"
)]
fn browser_goal_is_absent_when_not_enabled() {
    for control in [
        FloorControl::Off,
        FloorControl::On {
            goal: false,
            retire: false,
            renew: false,
        },
        FloorControl::On {
            goal: true,
            retire: false,
            renew: false,
        },
    ] {
        let fixture = Fixture::new();
        let mut settings = goal_settings(&fixture, false);
        settings.floor_control = control;
        let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (agent, goal) = paused_with_goal(&running, &fixture);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let (view, _) = snapshot(addr, &cookie, &csrf);
        let body = serde_json::to_vec(&goal_body(&request_id(1), agent, goal, NEW_TEXT)).unwrap();
        let unrouted = raw_post(addr, "/floor/unrouted", &cookie, &csrf, &body);
        assert!(!unrouted.is_empty());
        let routed = raw_post(addr, GOAL_ROUTE, &cookie, &csrf, &body);
        if control.goal() {
            assert!(
                String::from_utf8_lossy(&routed).starts_with("HTTP/1.1 200 OK"),
                "control: {}",
                String::from_utf8_lossy(&routed)
            );
            assert_eq!(view["goal"], json!({ "max_text_bytes": 2048 }));
            assert_eq!(browser_goals(&fixture, agent).len(), 1);
        } else {
            assert_eq!(
                routed,
                unrouted,
                "{control:?}: disabled is exactly unrouted: {}",
                String::from_utf8_lossy(&routed)
            );
            assert!(view.get("goal").is_none(), "{view}");
            assert!(browser_goals(&fixture, agent).is_empty());
        }
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }

    let root = tempfile::tempdir().expect("operator config root");
    let config = root.path().join("config.toml");
    let valid = "[budget]\nfloor_usd_per_day = 10\n[budget.per_turn_floor]\nusd = 0.01\n";
    let load = |source: String| {
        std::fs::write(&config, source).expect("config");
        load_operator(&config).map(|operator| operator.control)
    };
    assert_eq!(
        load(format!(
            "{valid}[floor.control]\nenabled = true\n[floor.goal]\nenabled = true\n"
        ))
        .expect("control: both switches with a ceiling and a reserve"),
        FloorControl::On {
            goal: true,
            retire: false,
            renew: false,
        }
    );
    assert_eq!(
        load(format!(
            "{valid}[floor.control]\nenabled = true\n[floor.goal]\nenabled = false\n"
        ))
        .expect("goal off"),
        FloorControl::On {
            goal: false,
            retire: false,
            renew: false,
        }
    );
    assert_eq!(
        load(valid.to_owned()).expect("absent"),
        FloorControl::Off,
        "absent is off"
    );
    assert_eq!(
        load("[floor.control]\nenabled = true\n[floor.goal]\nenabled = true\n".to_owned())
            .expect("goal change needs no budget"),
        FloorControl::On {
            goal: true,
            retire: false,
            renew: false,
        }
    );
    for (label, source) in [
        (
            "no [floor.control]",
            format!("{valid}[floor.goal]\nenabled = true\n"),
        ),
        (
            "[floor.control] off",
            format!("{valid}[floor.control]\nenabled = false\n[floor.goal]\nenabled = true\n"),
        ),
        (
            "an unknown key",
            format!(
                "{valid}[floor.control]\nenabled = true\n[floor.goal]\nenabled = true\nceiling = 5\n"
            ),
        ),
    ] {
        assert!(load(source).is_err(), "{label} is refused at load");
    }
}

/// On an attached agent: the change applies, the control gate holds it, and no `pty.write`, `Steer` or
/// hook reply carries the text — the agent's own prompt hook is answered as before. The first
/// Managed `TurnIssued` after the detach serves the new goal and carries its text.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one attach, change, hook, detach scenario across three daemon runs"
)]
fn browser_goal_on_attached_agent_writes_no_pty_or_hook_reply() {
    use common::hook_shim::{
        RecordingResolver, generated_handlers, handler, minted_token, payload, spawn_handler,
    };
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let parts = fake_seams(&settings);
    let tokens = parts.rng.tokens();
    let mut running = common::start(boot(&settings, parts).expect("boot the setup run"));
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[true_program()]),
    );
    let token = minted_token(&fixture, &tokens, agent);
    let turn = newest_turn(&fixture, agent);
    running.handle.ask(Ask::Pause(agent)).expect("local pause");
    end_turn(&running.handle, turn, Some(Usd::from_cents(1)));
    fixture.await_record("the held verdict", |record| {
        matches!(record, Record::PredicateResult { .. }).then_some(())
    });
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    seed(
        &fixture,
        [Record::Attached {
            agent,
            by: dudes_proto::Actor::System,
            hook_port: None,
            provenance: None,
        }],
    );

    let resolver = RecordingResolver::new(PeerProvenance::Unresolvable);
    let daemon = boot(
        &settings,
        seams_with(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
            Arc::clone(&resolver) as Arc<dyn PeerResolver>,
        ),
    )
    .expect("boot Floor");
    let socket = daemon.socket().to_path_buf();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    running
        .handle
        .ask(Ask::Resume(agent))
        .expect("local resume");
    let goal = folded(&fixture).agents[&agent].goal.expect("a goal");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, response) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_body(&request_id(1), agent, goal, NEW_TEXT),
    );
    let g2 = changed(&head, &response);
    let c = browser_goals(&fixture, agent)[0].0;
    resolver.set(PeerProvenance::Agent(agent));
    let generated = generated_handlers(agent, fixture.state.path());
    let (_, prompt) = spawn_handler(
        handler(&generated, "UserPromptSubmit"),
        &socket,
        &token,
        &payload("UserPromptSubmit"),
    );
    assert_eq!(prompt.status.code(), Some(0), "{prompt:?}");
    assert!(
        !String::from_utf8_lossy(&prompt.stdout).contains(NEW_TEXT),
        "no hook reply carries the text: {prompt:?}"
    );
    let _ = snapshot(addr, &cookie, &csrf);
    let changed_records = after(&fixture, c);
    assert!(
        !changed_records.iter().any(|(_, e)| matches!(
            &e.record,
            Record::TurnIssued { .. }
                | Record::PtyWriteRequested { .. }
                | Record::Steer { .. }
                | Record::Detached { .. }
        )),
        "the control gate holds the attached agent and nothing types the text: {changed_records:?}"
    );
    assert!(
        changed_records.iter().any(|(_, e)| matches!(
            &e.record,
            Record::LoopDecided {
                agent: who,
                decision: LoopDecision::Hold { .. },
                ..
            } if *who == agent
        )),
        "the change's decision is a Hold"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    seed(
        &fixture,
        [Record::Detached {
            agent,
            by: dudes_proto::Actor::System,
            provenance: None,
        }],
    );
    let detached = fixture
        .entries()
        .last()
        .map(|(lsn, _)| *lsn)
        .expect("seeded");
    let mut running =
        common::start(boot(&settings, fake_seams(&settings)).expect("boot after detach"));
    let task = fixture.await_record("the first turn after the detach", |record| match record {
        Record::TurnIssued {
            agent: who,
            goal,
            task,
            mode: dudes_types::Mode::Managed,
            ..
        } if *who == agent && *goal == g2 => Some(task.clone()),
        _ => None,
    });
    assert!(task.contains(NEW_TEXT), "{task}");
    assert!(
        after(&fixture, c)
            .iter()
            .filter(
                |(_, e)| matches!(&e.record, Record::TurnIssued { agent: who, .. } if *who == agent)
            )
            .all(|(lsn, _)| *lsn > detached),
        "no turn was issued before the detach"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

fn raw_mixed_goal_attribution_refusal_seams()
-> Seams<SharedHost, ScriptedRunner, TestClock, FakeRng> {
    Seams {
        host: SharedHost::new(),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(ScriptedRunner::default()),
        clock: Arc::new(TestClock::new()),
        rng: FakeRng::new(),
        resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
        adapters: Adapters::new([Box::new(ClaudeAdapter::default()) as Box<dyn Adapter>]),
        children: ChildEnds::new(),
        witness: Witness::default(),
    }
}

/// Only the two browser commands construct an attribution, each its own, so the goal index
/// refuses to replay a `GoalSet` carrying both, and the daemon does not start. Control: the same
/// durable hire and goal with only the onboarding attribution start.
#[test]
fn browser_goal_index_refuses_a_goal_set_with_both_attributions() {
    for both in [false, true] {
        let fixture = Fixture::new();
        let settings = goal_settings(&fixture, true);
        let agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("an agent id");
        let intended = Goal {
            completion_check: None,
            id: GoalId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW").expect("a goal id"),
            text: "ship".to_owned(),
            predicate: Predicate::Command {
                argv: vec![true_program().to_owned()],
                timeout_s: 10,
            },
            budget: GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
            hop_cap: 3,
            baseline: None,
        };
        let attempt_id = Hex32::new([7; 32]);
        let session_digest = Hex32::new([9; 32]);
        seed(
            &fixture,
            [
                Record::AgentHired {
                    character: None,
                    accent: None,
                    id: agent,
                    name: "Kim".to_owned(),
                    role: "engineer".to_owned(),
                    department: "engineering".to_owned(),
                    provider: Provider::Claude,
                    cwd: fixture.state.path().to_path_buf(),
                    budget: AgentBudget {
                        usd_per_day: Some(Usd::from_dollars(10)),
                        tokens_per_day: None,
                    },
                    sandbox: None,
                    isolation: Some(IsolationMode::Default),
                    max_turns: None,
                    max_budget: None,
                    trusted: false,
                    workspace: None,
                    worktree: None,
                    mcp_servers: Vec::new(),
                    browser_onboarding: Some(Box::new(BrowserOnboardingHire {
                        attempt_id,
                        request_digest: Hex32::new([8; 32]),
                        profile_id: "repo_1".to_owned(),
                        session_digest,
                        intended_goal: intended.clone(),
                    })),
                },
                Record::GoalSet {
                    goal_replacement: None,
                    agent,
                    goal: intended,
                    browser_onboarding: Some(BrowserOnboardingGoal {
                        attempt_id,
                        session_digest,
                    }),
                    browser_goal: both.then(|| {
                        Box::new(BrowserGoal {
                            kind: dudes_proto::record::BrowserGoalKind::Change,
                            request_id: Hex32::new([1; 32]),
                            request_digest: Hex32::new([2; 32]),
                            replaced_goal: GoalId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAX")
                                .expect("a goal id"),
                            reserved: Spend::default(),
                            session_digest,
                            check_chosen: false,
                        })
                    }),
                },
            ],
        );
        if both {
            let before = fixture.records();
            let booted = boot(&settings, raw_mixed_goal_attribution_refusal_seams());
            let error = format!("{:#}", booted.err().expect("replay refused"));
            assert!(
                error.contains("mixed replacement attributions"),
                "the goal index refused it: {error}"
            );
            assert_eq!(fixture.records(), before, "refusal appends no records");
        } else {
            let daemon =
                boot(&settings, fake_seams(&settings)).expect("control: one attribution replays");
            let mut running = common::start(daemon);
            assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
        }
    }
}

/// The fixture daemon's own `[budget]` and Hire switch, so a reload of this file applies nothing
/// else.
const RELOAD_BUDGET: &str = "[budget]\nfloor_usd_per_day = 100\n\n[budget.per_turn_floor]\nusd = 0.000001\ntokens = 1\n\n[floor.hire]\nenabled = true\n";

fn workspace_entry(id: &str, cwd: &std::path::Path) -> String {
    let program = serde_json::to_string(true_program()).expect("predicate TOML string");
    format!(
        "\n[[floor.onboarding.workspaces]]\nworkspace_id = '{id}'\nlabel = 'Workspace {id}'\ncompletion_check_label = 'Checks pass'\ncwd = '{}'\ndepartment = 'engineering'\nisolation = 'default'\nagent_usd_per_day = 10\ngoal_usd = 5\nhop_cap = 3\npredicate_argv = [{program}]\npredicate_timeout_s = 10\nmay_already_pass = true\n",
        cwd.display()
    )
}

fn reload_config(entries: &[(&str, &std::path::Path)]) -> String {
    let mut text = format!("{RELOAD_BUDGET}[memory]\ncondense_model_enabled = false\n");
    for (id, cwd) in entries {
        text.push_str(&workspace_entry(id, cwd));
    }
    text
}

/// A canonical repository directory under the fixture.
fn reload_repo(fixture: &Fixture, name: &str) -> PathBuf {
    let repo = fixture.state.path().join(name);
    std::fs::create_dir_all(repo.join(".git")).expect("repository marker");
    repo.canonicalize().expect("canonical repository")
}

/// Floor settings loaded from a real operator file, as `main` loads them.
fn reload_settings(fixture: &Fixture, text: &str) -> duded::Settings {
    let config = fixture.state.path().join("config.toml");
    std::fs::write(&config, text).expect("operator file");
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.enrollment = load_operator(&config).expect("valid start file").enrollment;
    settings.operator_config = Some(config);
    settings
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Rewrite the operator file and return the SHA-256 of exactly its bytes.
fn rewrite(settings: &duded::Settings, text: &str) -> String {
    std::fs::write(settings.operator_config.as_ref().expect("a file"), text).expect("rewrite");
    sha256_hex(text.as_bytes())
}

fn apply(sha256: &str, added: &[&str], removed: &[&str], changed: &[&str]) -> Value {
    json!({"apply": {"sha256": sha256, "added": added, "removed": removed, "changed": changed}})
}

/// One `floor.enrollment.reload` as the operator.
fn operator_reload(settings: &duded::Settings, params: Value) -> Result<Value, (i32, String)> {
    let credential = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("operator credential");
    let mut client = Client::connect(settings.socket()).expect("daemon socket");
    let reply = client
        .call_as_operator(
            Method::FloorEnrollmentReload,
            Some(params),
            OperatorCredential::new(credential).expect("credential"),
        )
        .expect("reload response");
    match (reply.result, reply.error) {
        (Some(result), None) => Ok(result),
        (_, Some(error)) => Err((error.code, error.message)),
        other => panic!("a response with neither result nor error: {other:?}"),
    }
}

/// The workspace ids a fresh snapshot offers, with each one's mark.
fn offered(addr: SocketAddr, cookie: &str, csrf: &str) -> Vec<(String, Value)> {
    let (frame, _) = snapshot(addr, cookie, csrf);
    frame["onboarding"]["workspaces"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|space| {
                    (
                        space["workspace_id"].as_str().expect("id").to_owned(),
                        space.get("reload_mark").cloned().unwrap_or(Value::Null),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// An open, subscribed stream that the test keeps reading after its snapshot.
struct OpenStream(TcpStream);

impl OpenStream {
    fn open(addr: SocketAddr, cookie: &str, csrf: &str) -> Self {
        let host = addr.to_string();
        let mut stream = TcpStream::connect(addr).expect("Floor listener");
        let request = format!(
            "GET /floor/stream HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: dudes-floor-v3, dudes-csrf.{csrf}\r\nCookie: dudes_floor_session={cookie}\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).expect("upgrade");
        assert!(read_head(&mut stream).starts_with("HTTP/1.1 101 Switching Protocols"));
        let subscribe = br#"{"type":"floor.subscribe"}"#;
        let mask = [1_u8, 2, 3, 4];
        let mut frame = vec![0x81, 0x80 | u8::try_from(subscribe.len()).expect("small")];
        frame.extend_from_slice(&mask);
        frame.extend(subscribe.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        stream.write_all(&frame).expect("subscribe");
        let mut open = Self(stream);
        assert_eq!(
            open.next_type(Duration::from_secs(5)).as_deref(),
            Some("floor.snapshot")
        );
        open
    }

    /// The next frame's `type` other than a heartbeat, or `None` once `bound` passes.
    fn next_type(&mut self, bound: Duration) -> Option<String> {
        self.next_frame(bound)
            .map(|frame| frame["type"].as_str().expect("typed frame").to_owned())
    }

    /// The next frame other than a heartbeat, or `None` once `bound` passes.
    fn next_frame(&mut self, bound: Duration) -> Option<Value> {
        let deadline = std::time::Instant::now() + bound;
        loop {
            let left = deadline.checked_duration_since(std::time::Instant::now())?;
            self.0
                .set_read_timeout(Some(left.max(Duration::from_millis(1))))
                .expect("read bound");
            let mut head = [0_u8; 2];
            match self.0.read_exact(&mut head) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return None;
                }
                Err(error) => panic!("stream frame: {error}"),
            }
            self.0
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("bound");
            let length = match head[1] & 0x7f {
                n @ 0..=125 => usize::from(n),
                126 => {
                    let mut bytes = [0; 2];
                    self.0.read_exact(&mut bytes).expect("u16 length");
                    usize::from(u16::from_be_bytes(bytes))
                }
                _ => {
                    let mut bytes = [0; 8];
                    self.0.read_exact(&mut bytes).expect("u64 length");
                    usize::try_from(u64::from_be_bytes(bytes)).expect("bounded")
                }
            };
            let mut payload = vec![0; length];
            self.0.read_exact(&mut payload).expect("payload");
            let value: Value = serde_json::from_slice(&payload).expect("frame JSON");
            if value["type"] != "floor.heartbeat" {
                return Some(value);
            }
        }
    }
}

fn boot_reload(
    settings: &duded::Settings,
) -> (common::Running, SocketAddr, duded::runtime::OnboardingPause) {
    let daemon = boot(
        settings,
        seams(
            settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let pause = daemon.onboarding_pause();
    (common::start(daemon), addr, pause)
}

fn memory_reload_file(after_kib: usize, keep: usize, enabled: bool, model: &str) -> String {
    format!(
        "{RELOAD_BUDGET}\n[memory]\ncondense_after_kib = {after_kib}\ncondense_keep_sections = {keep}\ncondense_model_enabled = {enabled}\ncondense_model = '{model}'\n"
    )
}

#[test]
fn reload_reports_each_memory_setting_against_nondefault_boot_state() {
    let fixture = Fixture::new();
    let initial = memory_reload_file(17, 3, false, "fixture-small");
    let mut settings = reload_settings(&fixture, &initial);
    settings.condense_model_enabled = false;
    settings.condense_model = Some("fixture-small".into());
    let (mut running, _, _) = boot_reload(&settings);
    let checked = operator_reload(&settings, json!({})).expect("matching initial file");
    assert_eq!(checked["not_applied"], json!([]));
    let mut observed = Vec::new();
    for changed in [
        memory_reload_file(32, 3, false, "fixture-small"),
        memory_reload_file(17, 13, false, "fixture-small"),
        memory_reload_file(17, 3, true, "fixture-small"),
        memory_reload_file(17, 3, false, "fixture-other"),
        memory_reload_file(32, 13, true, "fixture-other"),
        RELOAD_BUDGET.to_owned(),
    ] {
        rewrite(&settings, &changed);
        observed.push(operator_reload(&settings, json!({})).expect("check")["not_applied"].clone());
        rewrite(&settings, &initial);
        assert_eq!(
            operator_reload(&settings, json!({})).expect("restored")["not_applied"],
            json!([]),
        );
    }
    assert_eq!(observed, vec![json!(["memory"]); 6]);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn memory_restart_comparison_uses_the_hashed_bytes_before_a_file_swap() {
    let fixture = Fixture::new();
    let initial = memory_reload_file(17, 3, false, "fixture-small");
    let mut settings = reload_settings(&fixture, &initial);
    settings.condense_model_enabled = false;
    settings.condense_model = Some("fixture-small".into());
    let (mut running, _, pause) = boot_reload(&settings);
    let changed = memory_reload_file(32, 3, false, "fixture-small");
    let hashed = rewrite(&settings, &changed);
    pause.arm(OnboardingPausePoint::AfterReloadHash);
    let held_settings = settings.clone();
    let held = std::thread::spawn(move || operator_reload(&held_settings, json!({})));
    assert!(pause.wait_reached(Duration::from_secs(10)));
    rewrite(&settings, &initial);
    pause.release();
    let result = held.join().expect("caller").expect("reload result");
    assert_eq!(result["sha256"], hashed);
    assert_eq!(result["not_applied"], json!(["memory"]));
    assert_eq!(
        operator_reload(&settings, json!({})).expect("current file")["not_applied"],
        json!([])
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The primary reload test: an applied reload reaches an open Floor stream within its session, the new
/// workspace carries its mark, a hire presenting that mark's revision uses the new entry, and no
/// reload appends anything.
#[test]
fn browser_enrollment_view_follows_an_operator_reload() {
    let fixture = Fixture::new();
    let (repo_1, repo_2) = (
        reload_repo(&fixture, "repo_1"),
        reload_repo(&fixture, "repo_2"),
    );
    let settings = reload_settings(&fixture, &reload_config(&[("repo_1", &repo_1)]));
    let (mut running, addr, _) = boot_reload(&settings);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    assert_eq!(
        offered(addr, &cookie, &csrf),
        vec![("repo_1".to_owned(), Value::Null)]
    );
    let mut open = OpenStream::open(addr, &cookie, &csrf);
    let journal = fixture.records().len();

    let sha = rewrite(
        &settings,
        &reload_config(&[("repo_1", &repo_1), ("repo_2", &repo_2)]),
    );
    let result = operator_reload(&settings, apply(&sha, &["repo_2"], &[], &[])).expect("applied");
    assert_eq!(
        result,
        json!({
            "applied": true, "sha256": sha,
            "path": settings.operator_config.as_ref().and_then(|p| p.to_str()),
            "workspaces": ["repo_1", "repo_2"], "added": ["repo_2"], "removed": [],
            "changed": [], "switches": [], "not_applied": []
        })
    );
    assert_eq!(fixture.records().len(), journal, "a reload appends nothing");
    assert_eq!(
        open.next_type(Duration::from_secs(5)).as_deref(),
        Some("floor.resync"),
        "the open stream is told to take a fresh snapshot"
    );

    let now = offered(addr, &cookie, &csrf);
    assert_eq!(now[0], ("repo_1".to_owned(), Value::Null));
    assert_eq!(now[1].0, "repo_2");
    assert_eq!(now[1].1["kind"], "added");
    assert_eq!(now[1].1["revision"], "1");
    let at = now[1].1["at"].as_str().expect("a mark time");
    assert!(
        at.len() == 20 && at.ends_with('Z') && at.as_bytes()[10] == b'T',
        "RFC 3339 to the second: {at}"
    );

    let (head, body) = post_onboard(
        addr,
        &cookie,
        &csrf,
        &json!({
            "attempt_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "workspace_id": "repo_2", "workspace_revision": "1",
            "name": "Kim", "role": "engineer", "goal_text": "ship"
        }),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {body}");
    assert!(fixture.records().iter().any(|record| matches!(
        record,
        Record::AgentHired { cwd, browser_onboarding: Some(meta), .. }
            if *cwd == repo_2 && meta.profile_id == "repo_2"
    )));

    let goal = GoalId::parse(body["goal"].as_str().expect("goal id")).expect("goal id");
    issued_goal_turn(&fixture, goal);
    running.handle.doctor().unwrap();
    let journal = fixture.records().len();
    let sha = rewrite(&settings, &reload_config(&[("repo_2", &repo_2)]));
    let result = operator_reload(&settings, apply(&sha, &[], &["repo_1"], &[])).expect("applied");
    assert_eq!(result["removed"], json!(["repo_1"]));
    assert_eq!(result["workspaces"], json!(["repo_2"]));
    assert_eq!(
        fixture.records().len(),
        journal,
        "a removal appends nothing"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_workspace_label_survives_enrollment_change_removal_and_replay() {
    let fixture = Fixture::new();
    let repo = reload_repo(&fixture, "WORKSPACE-SECRET");
    let initial_config = reload_config(&[("repo_1", &repo)]);
    let mut settings = reload_settings(&fixture, &initial_config);
    let (mut running, addr, _) = boot_reload(&settings);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, body) = post_onboard(
        addr,
        &cookie,
        &csrf,
        &json!({
            "attempt_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "workspace_id": "repo_1", "name": "Kim", "role": "engineer", "goal_text": "ship"
        }),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {body}");
    let expected = json!({ "id": "repo_1", "label": "Workspace repo_1" });
    let workspace_of = |addr, cookie: &str, csrf: &str| {
        let (view, _) = snapshot(addr, cookie, csrf);
        let agents = &view["projection"]["agents"];
        assert_eq!(agents.as_array().unwrap().len(), 1);
        assert!(
            !serde_json::to_string(agents)
                .unwrap()
                .contains("WORKSPACE-SECRET")
        );
        agents[0]["workspace"].clone()
    };
    assert_eq!(workspace_of(addr, &cookie, &csrf), expected);
    assert!(fixture.records().iter().any(|record| matches!(record,
        Record::AgentHired { workspace: Some(workspace), browser_onboarding: Some(_), .. }
            if workspace.id == "repo_1" && workspace.label == "Workspace repo_1"
    )));
    let changed =
        initial_config.replace("label = 'Workspace repo_1'", "label = 'Renamed workspace'");
    let sha = rewrite(&settings, &changed);
    operator_reload(&settings, apply(&sha, &[], &[], &["repo_1"])).unwrap();
    assert_eq!(workspace_of(addr, &cookie, &csrf), expected);
    let sha = rewrite(&settings, RELOAD_BUDGET);
    operator_reload(&settings, apply(&sha, &[], &["repo_1"], &[])).unwrap();
    assert_eq!(workspace_of(addr, &cookie, &csrf), expected);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    settings.enrollment = load_operator(settings.operator_config.as_ref().unwrap())
        .unwrap()
        .enrollment;
    let (mut replayed, addr, _) = boot_reload(&settings);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    assert_eq!(workspace_of(addr, &cookie, &csrf), expected);
    assert_eq!(
        replayed.stop(ExitCause::ShutdownRpc),
        ExitCause::ShutdownRpc
    );
}

#[cfg(unix)]
#[test]
fn enrollment_reload_refuses_a_fifo_and_an_oversized_file() {
    let fixture = Fixture::new();
    let (repo_1, repo_2) = (
        reload_repo(&fixture, "repo_1"),
        reload_repo(&fixture, "repo_2"),
    );
    let settings = reload_settings(&fixture, &reload_config(&[("repo_1", &repo_1)]));
    let (mut running, _, _) = boot_reload(&settings);
    let config = settings.operator_config.as_ref().expect("config path");
    std::fs::remove_file(config).expect("remove regular config");
    nix::unistd::mkfifo(
        config,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("create a FIFO at the config path");

    let (send, receive) = mpsc::channel();
    let in_thread = settings.clone();
    let reload = std::thread::spawn(move || {
        let answer = operator_reload(&in_thread, json!({}));
        send.send(answer).expect("test receives reload result");
    });
    let answer = receive.recv_timeout(Duration::from_secs(2));
    if matches!(&answer, Err(mpsc::RecvTimeoutError::Timeout)) {
        let writer = nix::fcntl::open(
            config,
            nix::fcntl::OFlag::O_WRONLY | nix::fcntl::OFlag::O_NONBLOCK,
            nix::sys::stat::Mode::empty(),
        )
        .expect("release the blocked FIFO reader");
        drop(writer);
    }
    reload.join().expect("reload thread");
    let (code, message) = answer
        .expect("a FIFO reload must return without waiting for a writer")
        .expect_err("a FIFO is not operator configuration");
    assert_eq!(code, -32602);
    assert!(message.contains("not a regular file"), "{message}");

    std::fs::remove_file(config).expect("remove FIFO");
    let mut oversized = reload_config(&[("repo_1", &repo_1)]);
    oversized.push('#');
    oversized.push_str(&"x".repeat(1_048_576));
    oversized.push('\n');
    std::fs::write(config, oversized).expect("large valid TOML");
    let (code, message) =
        operator_reload(&settings, json!({})).expect_err("an oversized file is refused");
    assert_eq!(code, -32602);
    assert!(message.contains("1 MiB"), "{message}");

    let sha = rewrite(
        &settings,
        &reload_config(&[("repo_1", &repo_1), ("repo_2", &repo_2)]),
    );
    let result = operator_reload(&settings, apply(&sha, &["repo_2"], &[], &[]))
        .expect("later regular reload applies");
    assert_eq!(result["applied"], true);
    assert_eq!(result["workspaces"], json!(["repo_1", "repo_2"]));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Each way an `apply` is declined, or refused, leaves the running set as it was; the control
/// applies.
#[test]
fn enrollment_reload_apply_is_bound_to_the_file_and_the_change() {
    let fixture = Fixture::new();
    let (repo_1, repo_2, repo_3) = (
        reload_repo(&fixture, "repo_1"),
        reload_repo(&fixture, "repo_2"),
        reload_repo(&fixture, "repo_3"),
    );
    let start = reload_config(&[("repo_1", &repo_1)]);
    let settings = reload_settings(&fixture, &start);
    let (mut running, addr, pause) = boot_reload(&settings);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let only_repo_1 = vec![("repo_1".to_owned(), Value::Null)];
    let with_repo_2 = reload_config(&[("repo_1", &repo_1), ("repo_2", &repo_2)]);

    let sha = rewrite(&settings, &with_repo_2);
    let stale = sha256_hex(start.as_bytes());
    let result = operator_reload(&settings, apply(&stale, &["repo_2"], &[], &[])).expect("answer");
    assert_eq!(result["applied"], false);
    assert_eq!(result["refused"], "file_changed");
    assert_eq!(result["sha256"], sha.as_str(), "the real hash is reported");
    assert_eq!(
        result["added"],
        json!(["repo_2"]),
        "the real change is reported"
    );
    assert_eq!(result["workspaces"], json!(["repo_1"]));
    assert_eq!(offered(addr, &cookie, &csrf), only_repo_1);

    let sha = rewrite(
        &settings,
        &reload_config(&[("repo_1", &repo_1), ("repo_3", &repo_3)]),
    );
    let result = operator_reload(&settings, apply(&sha, &["repo_2"], &[], &[])).expect("answer");
    assert_eq!(result["refused"], "unexpected_changes");
    assert_eq!(
        result["added"],
        json!(["repo_3"]),
        "the real change is shown"
    );
    assert_eq!(offered(addr, &cookie, &csrf), only_repo_1);

    #[cfg(unix)]
    {
        let link = fixture.state.path().join("link_to_repo_2");
        std::os::unix::fs::symlink(&repo_2, &link).expect("planted symlink");
        let text = reload_config(&[("repo_1", &repo_1), ("repo_2", &link)]);
        let sha = rewrite(&settings, &text);
        let (code, message) = operator_reload(&settings, apply(&sha, &["repo_2"], &[], &[]))
            .expect_err("a non-canonical cwd is refused");
        assert_eq!(code, -32602, "InvalidParams");
        assert!(
            message.contains("workspace `repo_2` whose cwd is not its canonical form"),
            "{message}"
        );
        assert_eq!(offered(addr, &cookie, &csrf), only_repo_1);
    }

    let older = rewrite(&settings, &with_repo_2);
    pause.arm(OnboardingPausePoint::AfterReloadHash);
    let held_settings = settings.clone();
    let held = std::thread::spawn(move || {
        operator_reload(&held_settings, apply(&older, &["repo_2"], &[], &[]))
    });
    assert!(
        pause.wait_reached(Duration::from_secs(10)),
        "the older reload holds after hashing"
    );
    let newer = rewrite(&settings, &format!("{start}# a comment, and no change\n"));
    let result = operator_reload(&settings, apply(&newer, &[], &[], &[])).expect("newer applies");
    assert_eq!(result["applied"], true);
    pause.release();
    let result = held.join().expect("the held caller").expect("answer");
    assert_eq!(result["refused"], "superseded");
    assert_eq!(
        result["added"],
        json!(["repo_2"]),
        "its own change still matched"
    );
    assert_eq!(offered(addr, &cookie, &csrf), only_repo_1);

    let sha = rewrite(&settings, &with_repo_2);
    let result = operator_reload(&settings, apply(&sha, &["repo_2"], &[], &[])).expect("applied");
    assert_eq!(result["applied"], true);
    assert_eq!(
        offered(addr, &cookie, &csrf)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        ["repo_1", "repo_2"]
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// A reload of a file that declares an MCP server whose variable this daemon lacks still applies
/// its enrollment change, and names `mcp.servers` as needing a restart. Mutant: judge the
/// variables on reload, which would refuse the whole file.
#[test]
fn a_reload_applies_enrollment_whatever_the_mcp_variables() {
    let fixture = Fixture::new();
    let (repo_1, repo_2) = (
        reload_repo(&fixture, "repo_1"),
        reload_repo(&fixture, "repo_2"),
    );
    let settings = reload_settings(&fixture, &reload_config(&[("repo_1", &repo_1)]));
    let (mut running, addr, _) = boot_reload(&settings);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let text = format!(
        "{}[mcp.servers.memory]\nurl = 'https://memory.example.com/mcp'\nbearer_env = 'MEMORY_BEARER_NEVER_SET'\n",
        reload_config(&[("repo_1", &repo_1), ("repo_2", &repo_2)])
    );
    let sha = rewrite(&settings, &text);
    let result = operator_reload(&settings, apply(&sha, &["repo_2"], &[], &[])).expect("applied");
    assert_eq!(result["applied"], true);
    assert_eq!(result["not_applied"], json!(["mcp.servers"]));
    assert_eq!(
        offered(addr, &cookie, &csrf)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        ["repo_1", "repo_2"]
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// A page that saw `repo_1` before a reload re-pointed it cannot hire into the new folder.
#[test]
fn browser_hire_is_bound_to_the_workspace_revision_it_saw() {
    let fixture = Fixture::new();
    let (repo_1, moved) = (
        reload_repo(&fixture, "repo_1"),
        reload_repo(&fixture, "moved"),
    );
    let settings = reload_settings(&fixture, &reload_config(&[("repo_1", &repo_1)]));
    let (mut running, addr, _) = boot_reload(&settings);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    assert_eq!(
        offered(addr, &cookie, &csrf),
        vec![("repo_1".to_owned(), Value::Null)]
    );

    let sha = rewrite(&settings, &reload_config(&[("repo_1", &moved)]));
    let result = operator_reload(&settings, apply(&sha, &[], &[], &["repo_1"])).expect("applied");
    assert_eq!(
        result["changed"],
        json!([{"id": "repo_1", "fields": ["cwd"]}])
    );

    let journal = fixture.records().len();
    let request = json!({
        "attempt_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id": "repo_1", "name": "Kim", "role": "engineer", "goal_text": "ship"
    });
    let (head, body) = post_onboard(addr, &cookie, &csrf, &request);
    assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
    assert_eq!(
        body,
        json!({
            "attempt_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "state": "refused", "reason": "profile_changed"
        })
    );
    assert_eq!(
        fixture.records().len(),
        journal,
        "a stale revision appends nothing"
    );

    let mut current = request;
    current["workspace_revision"] = json!("1");
    let (head, body) = post_onboard(addr, &cookie, &csrf, &current);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {body}");
    assert!(fixture.records().iter().any(|record| matches!(
        record,
        Record::AgentHired { cwd, browser_onboarding: Some(_), .. } if *cwd == moved
    )));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Only the operator reloads. An agent presenting its own token, a peer presenting
/// nothing, and a peer in an agent's tree presenting the operator credential are each refused.
#[test]
fn enrollment_reload_is_operator_only() {
    let fixture = Fixture::new();
    let (repo_1, repo_2) = (
        reload_repo(&fixture, "repo_1"),
        reload_repo(&fixture, "repo_2"),
    );
    let settings = reload_settings(&fixture, &reload_config(&[("repo_1", &repo_1)]));
    let column = common::ScriptedColumn::new(dudes_rpc::auth::PeerProvenance::NoRecordedAgent);
    let parts = seams_with(
        &settings,
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
        Arc::clone(&column) as Arc<dyn dudes_rpc::auth::PeerResolver>,
    );
    let tokens = parts.rng.tokens();
    let daemon = boot(&settings, parts).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::false_program()]),
    );
    let token_hash = fixture.await_record("HookTokenMinted", |record| match record {
        Record::HookTokenMinted { token_hash, .. } => Some(token_hash.clone()),
        _ => None,
    });
    let token = tokens.token_of(&token_hash);
    running.handle.doctor().unwrap();
    let sha = rewrite(
        &settings,
        &reload_config(&[("repo_1", &repo_1), ("repo_2", &repo_2)]),
    );
    let params = apply(&sha, &["repo_2"], &[], &[]);

    column.set(dudes_rpc::auth::PeerProvenance::Agent(agent));
    let mut with_token = params.clone();
    with_token["token"] = json!(token.expose());
    let agent_reply = Client::connect(settings.socket())
        .expect("socket")
        .call(Method::FloorEnrollmentReload, Some(with_token))
        .expect("agent reply");
    let operator_in_tree = operator_reload(&settings, params.clone());
    column.set(dudes_rpc::auth::PeerProvenance::NoRecordedAgent);
    let unproven = Client::connect(settings.socket())
        .expect("socket")
        .call(Method::FloorEnrollmentReload, Some(params))
        .expect("unproven reply");
    for (who, code) in [
        (
            "an agent's own token",
            agent_reply.error.map(|error| error.code),
        ),
        (
            "the operator credential inside an agent's tree",
            operator_in_tree.err().map(|e| e.0),
        ),
        (
            "a peer presenting nothing",
            unproven.error.map(|error| error.code),
        ),
    ] {
        assert_eq!(code, Some(-32001), "{who} is Unauthorized");
    }
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    assert_eq!(
        offered(addr, &cookie, &csrf),
        vec![("repo_1".to_owned(), Value::Null)],
        "the old set is still offered, with no mark"
    );
    let applied = operator_reload(&settings, apply(&sha, &["repo_2"], &[], &[]))
        .expect("control: the operator outside every agent's tree applies");
    assert_eq!(applied["applied"], true);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// `repo_1`'s directory becomes a symlink to another repository after start. A reload that leaves
/// its entry unchanged keeps the start-time canonical `cwd`, so the entry drops out of the offer
/// rather than being re-pointed with no mark and no resync.
#[cfg(unix)]
#[test]
fn enrollment_reload_ignores_a_symlink_in_an_unchanged_entry() {
    let fixture = Fixture::new();
    let (repo_1, repo_2, other) = (
        reload_repo(&fixture, "repo_1"),
        reload_repo(&fixture, "repo_2"),
        reload_repo(&fixture, "other"),
    );
    let settings = reload_settings(&fixture, &reload_config(&[("repo_1", &repo_1)]));
    let (mut running, addr, _) = boot_reload(&settings);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    assert_eq!(
        offered(addr, &cookie, &csrf),
        vec![("repo_1".to_owned(), Value::Null)]
    );

    std::fs::rename(&repo_1, fixture.state.path().join("repo_1_moved")).expect("move away");
    std::os::unix::fs::symlink(&other, &repo_1).expect("planted symlink");
    let sha = rewrite(
        &settings,
        &reload_config(&[("repo_1", &repo_1), ("repo_2", &repo_2)]),
    );
    let result = operator_reload(&settings, apply(&sha, &["repo_2"], &[], &[])).expect("applied");
    assert_eq!(result["applied"], true);
    assert_eq!(
        result["changed"],
        json!([]),
        "an unchanged entry is not a change"
    );
    assert_eq!(
        offered(addr, &cookie, &csrf)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        ["repo_2"],
        "repo_1 keeps its start-time cwd, which no longer canonicalizes to itself"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// A managed launch of `.0`, bare or absolute, with the daemon's environment as the child's, so
/// the launch looks `.0` up on the `PATH` the provider check searches.
#[cfg(unix)]
#[derive(Debug)]
struct ProgramAdapter(OsString);

#[cfg(unix)]
impl Adapter for ProgramAdapter {
    fn provider(&self) -> Provider {
        Provider::Claude
    }
    fn program(&self) -> &std::ffi::OsStr {
        &self.0
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
            argv: vec![self.0.clone()],
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

/// The real host launching [`ProgramAdapter`]`(program)`, with `goals` passing predicates.
#[cfg(unix)]
fn program_seams(
    settings: &duded::Settings,
    program: OsString,
    goals: usize,
) -> Seams<RealHost, ScriptedRunner, TestClock, common::FixtureRng> {
    let children = ChildEnds::new();
    Seams {
        host: RealHost::watching(Arc::new(HostStreams(Arc::clone(&children)))),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(ScriptedRunner::scripted(
            std::iter::repeat_with(common::passes).take(goals),
        )),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(settings, FakeRng::new()),
        resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
        adapters: [Box::new(ProgramAdapter(program)) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children,
        witness: Witness::default(),
    }
}

/// An executable script at `path` that appends the path it was run as to `marks`.
#[cfg(unix)]
fn install_prog(path: &std::path::Path, marks: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
    std::fs::write(
        path,
        format!("#!/bin/sh\necho \"$0\" >> '{}'\n", marks.display()),
    )
    .expect("write the program");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .expect("make it executable");
}

/// The default configuration's Floor over `settings`: no enrollment and no opt-in sibling.
fn with_floor(mut settings: duded::Settings) -> duded::Settings {
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings
}

/// One managed turn of a new agent in `workspace`, and how it ended.
#[cfg(unix)]
fn one_launch(
    handle: &duded::Handle,
    fixture: &Fixture,
    workspace: &std::path::Path,
) -> TurnOutcome {
    std::fs::create_dir_all(workspace).expect("the workspace");
    let agent = common::hire(handle, workspace);
    common::set_goal(handle, common::goal_params(agent, &[true_program()]));
    fixture.await_record("TurnEnded", |record| match record {
        Record::TurnEnded {
            agent: ended,
            outcome,
            ..
        } if *ended == agent => outcome.clone(),
        _ => None,
    })
}

/// Long enough that the next subscription reuses no earlier check.
const PAST_REUSE: Duration = Duration::from_millis(1_150);

#[cfg(unix)]
#[test]
fn browser_daemon_view_reports_the_launch_check_and_home() {
    let home = tempfile::tempdir().expect("a home");
    let outside = tempfile::tempdir().expect("a directory outside the home");
    let fixture = Fixture {
        state: tempfile::Builder::new()
            .prefix("state")
            .rand_bytes(0)
            .tempdir_in(home.path())
            .expect("the state directory under the home"),
        runtime: tempfile::tempdir().expect("a runtime directory"),
    };
    let marks = outside.path().join("marks");
    let markers = outside.path().join("markers");
    let shell_marks = outside.path().join("shell-marks");
    for shell in ["which", "where", "sh", "bash", "zsh", "env"] {
        install_prog(&markers.join(shell), &shell_marks);
    }
    let bin_a = home.path().join("bin-a");
    let bin_b = outside.path().join("bin-b");
    install_prog(&bin_a.join("prog"), &marks);
    std::fs::create_dir_all(&bin_b).expect("bin-b");
    let mut settings = with_floor(fixture.settings());
    settings.daemon_env = vec![
        EnvVar::new(
            "PATH",
            std::env::join_paths([&markers, &bin_a, &bin_b]).expect("a PATH"),
        ),
        EnvVar::new("HOME", home.path()),
    ];
    let daemon = boot(
        &settings,
        program_seams(&settings, OsString::from("prog"), 3),
    )
    .expect("boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);

    let (frame, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(
        frame["daemon"],
        json!({"state_dir":"~/state","providers":[
            {"provider":"claude","program":"prog","found":true,"path":"~/bin-a/prog"}
        ]})
    );
    one_launch(&running.handle, &fixture, &home.path().join("work-a"));
    assert_eq!(
        std::fs::read_to_string(&marks).expect("marks"),
        format!("{}\n", bin_a.join("prog").display()),
        "the launch ran the file the view reported"
    );

    std::fs::remove_file(bin_a.join("prog")).expect("remove bin-a/prog");
    install_prog(&bin_b.join("prog"), &marks);
    std::fs::write(&marks, "").expect("clear the marks");
    std::thread::sleep(PAST_REUSE);
    let (frame, _) = snapshot(addr, &cookie, &csrf);
    let outside_prog = bin_b.join("prog").to_str().expect("UTF-8").to_owned();
    assert_eq!(
        frame["daemon"]["providers"],
        json!([{"provider":"claude","program":"prog","found":true,"path":outside_prog}]),
        "a path outside the home is shown in full, with no restart"
    );
    one_launch(&running.handle, &fixture, &home.path().join("work-b"));
    assert_eq!(
        std::fs::read_to_string(&marks).expect("marks"),
        format!("{outside_prog}\n")
    );

    std::fs::remove_file(bin_b.join("prog")).expect("remove bin-b/prog");
    std::fs::write(&marks, "").expect("clear the marks");
    std::thread::sleep(PAST_REUSE);
    let (frame, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(
        frame["daemon"]["providers"],
        json!([{"provider":"claude","program":"prog","found":false}])
    );
    let outcome = one_launch(&running.handle, &fixture, &home.path().join("work-c"));
    assert!(
        matches!(outcome, TurnOutcome::NotLaunched { .. }),
        "a program the view reports missing is not launched: {outcome:?}"
    );
    assert_eq!(std::fs::read_to_string(&marks).expect("marks"), "");
    assert_eq!(
        std::fs::read_to_string(&shell_marks).ok(),
        None,
        "no check ran a shell or a lookup tool from the head of the daemon's PATH"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// One shape of `PATH` or configured command, and the file the check must report for it.
#[cfg(unix)]
struct Shape {
    name: &'static str,
    program: OsString,
    path: OsString,
    expected: Option<PathBuf>,
}

/// A `prog` in `root/acl` whose ACL denies the daemon's own user execute while its mode bits allow
/// it, ahead of `exec`. Only where the platform allows that without root.
#[cfg(target_os = "macos")]
#[expect(
    clippy::disallowed_types,
    reason = "the fixture sets an ACL, which the standard library cannot"
)]
fn acl_shapes(
    root: &std::path::Path,
    marks: &std::path::Path,
    exec: &std::path::Path,
) -> Vec<Shape> {
    let acl = root.join("acl");
    install_prog(&acl.join("prog"), marks);
    let user = std::env::var("USER").expect("the test's user name");
    let status = std::process::Command::new("/bin/chmod")
        .arg("+a")
        .arg(format!("{user} deny execute"))
        .arg(acl.join("prog"))
        .status()
        .expect("run chmod");
    assert!(status.success(), "chmod +a refused");
    vec![Shape {
        name: "a prog whose ACL denies execute before an executable one",
        program: OsString::from("prog"),
        path: std::env::join_paths([&acl, exec]).expect("a PATH"),
        expected: Some(exec.join("prog")),
    }]
}

#[cfg(all(unix, not(target_os = "macos")))]
fn acl_shapes(_: &std::path::Path, _: &std::path::Path, _: &std::path::Path) -> Vec<Shape> {
    Vec::new()
}

/// Each shape under `root`, every candidate `prog` appending the path it ran as to `marks`.
#[cfg(unix)]
fn path_shapes(root: &std::path::Path, marks: &std::path::Path) -> Vec<Shape> {
    use std::os::unix::fs::PermissionsExt as _;

    let abs = root.join("abs");
    install_prog(&abs.join("prog"), marks);
    let exec = root.join("exec");
    install_prog(&exec.join("prog"), marks);
    let mode = root.join("mode");
    install_prog(&mode.join("prog"), marks);
    std::fs::set_permissions(mode.join("prog"), std::fs::Permissions::from_mode(0o644))
        .expect("not executable");
    let dir = root.join("dir");
    std::fs::create_dir_all(dir.join("prog")).expect("a directory named prog");
    let link = root.join("link");
    install_prog(&root.join("target").join("real"), marks);
    std::fs::create_dir_all(&link).expect("link");
    std::os::unix::fs::symlink(root.join("target").join("real"), link.join("prog"))
        .expect("a symlink to an executable");
    let dangling = root.join("dangling");
    std::fs::create_dir_all(&dangling).expect("dangling");
    std::os::unix::fs::symlink(root.join("nowhere"), dangling.join("prog"))
        .expect("a dangling symlink");
    let none = root.join("none");
    let join = |entries: &[&std::path::Path]| std::env::join_paths(entries).expect("a PATH");
    let bare = || OsString::from("prog");

    [
        Shape {
            name: "relative and empty entries before an absolute one",
            program: bare(),
            path: join(&[".".as_ref(), "".as_ref(), &abs]),
            expected: Some(abs.join("prog")),
        },
        Shape {
            name: "relative and empty entries and no absolute one holding prog",
            program: bare(),
            path: join(&[".".as_ref(), "".as_ref(), &none]),
            expected: None,
        },
        Shape {
            name: "a mode-0644 prog before an executable one",
            program: bare(),
            path: join(&[&mode, &exec]),
            expected: Some(exec.join("prog")),
        },
        Shape {
            name: "a directory named prog before an executable file",
            program: bare(),
            path: join(&[&dir, &exec]),
            expected: Some(exec.join("prog")),
        },
        Shape {
            name: "a symlink to an executable",
            program: bare(),
            path: join(&[&link]),
            expected: Some(link.join("prog")),
        },
        Shape {
            name: "a dangling symlink",
            program: bare(),
            path: join(&[&dangling]),
            expected: None,
        },
        Shape {
            name: "an absolute configured command that is present",
            program: abs.join("prog").into_os_string(),
            path: join(&[&none]),
            expected: Some(abs.join("prog")),
        },
        Shape {
            name: "an absolute configured command that is absent",
            program: root.join("gone").join("prog").into_os_string(),
            path: join(&[&abs]),
            expected: None,
        },
    ]
    .into_iter()
    .chain(acl_shapes(root, marks, &exec))
    .collect()
}

#[cfg(unix)]
#[test]
fn browser_daemon_view_agrees_with_the_launch_on_each_path_shape() {
    let root = tempfile::tempdir().expect("a scene");
    let root = root.path();
    let marks = root.join("marks");
    let workspace = root.join("work");
    install_prog(&workspace.join("prog"), &marks);
    for shape in &path_shapes(root, &marks) {
        let name = shape.name;
        std::fs::write(&marks, "").expect("clear the marks");
        let fixture = Fixture::new();
        let mut settings = with_floor(fixture.settings());
        settings.daemon_env = vec![EnvVar::new("PATH", &shape.path)];
        let daemon = boot(
            &settings,
            program_seams(&settings, shape.program.clone(), 1),
        )
        .expect("boot");
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let (frame, _) = snapshot(addr, &cookie, &csrf);
        let row = match &shape.expected {
            Some(path) => json!({"provider":"claude","program":"prog","found":true,
                "path":path.to_str().expect("UTF-8")}),
            None => json!({"provider":"claude","program":"prog","found":false}),
        };
        assert_eq!(
            frame["daemon"]["providers"],
            json!([row]),
            "{name}: the view"
        );
        let outcome = one_launch(&running.handle, &fixture, &workspace);
        let ran = std::fs::read_to_string(&marks).expect("marks");
        if let Some(path) = &shape.expected {
            assert_eq!(
                ran,
                format!("{}\n", path.display()),
                "{name}: the launch ran the file the view reported, and nothing planted"
            );
        } else {
            assert!(
                matches!(outcome, TurnOutcome::NotLaunched { .. }),
                "{name}: {outcome:?}"
            );
            assert_eq!(ran, "", "{name}: nothing ran, the planted prog included");
        }
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

/// The default configuration's Floor on the fake host, with a session: the daemon's provider
/// check, the running daemon, the Floor's address, and the session's cookie and CSRF nonce.
fn default_floor(
    settings: &duded::Settings,
) -> (
    duded::provider_check::ProviderCheck,
    common::Running,
    SocketAddr,
    String,
    String,
) {
    let daemon = boot(
        settings,
        seams(
            settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot Floor");
    let check = daemon.provider_check();
    let addr = daemon.floor_addr().expect("Floor address");
    let running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(settings).bootstrap);
    (check, running, addr, cookie, csrf)
}

/// Whether a record `found` accepts reaches the journal within `bound`.
fn journaled_within(fixture: &Fixture, bound: Duration, found: impl Fn(&Record) -> bool) -> bool {
    let deadline = std::time::Instant::now() + bound;
    loop {
        if fixture.records().iter().any(&found) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn browser_daemon_view_discloses_nothing_else() {
    let root = tempfile::tempdir().expect("a root");
    let root = root.path();
    let home = root.join("private-home-sentinel");
    let runtime = root.join("private-runtime-sentinel");
    std::fs::create_dir_all(&home).expect("the home");
    let frames = |state_dir: PathBuf, runtime_dir: PathBuf| -> String {
        let fixture = Fixture::new();
        let mut settings = with_floor(fixture.settings());
        std::fs::create_dir_all(&state_dir).expect("the state directory");
        settings.state_dir = state_dir;
        settings.runtime_dir = runtime_dir;
        let path = settings.daemon_env[0].expose_value().to_os_string();
        settings.daemon_env = vec![
            EnvVar::new(
                "PATH",
                std::env::join_paths(
                    [root.join("private-path-sentinel")]
                        .into_iter()
                        .chain(std::env::split_paths(&path)),
                )
                .expect("a PATH"),
            ),
            EnvVar::new("HOME", &home),
            EnvVar::new("PRIVATE_VALUE", "private-env-sentinel"),
        ];
        let (_, mut running, addr, cookie, csrf) = default_floor(&settings);
        let (frame, bytes) = snapshot(addr, &cookie, &csrf);
        assert_eq!(
            frame["daemon"]["providers"][0]["found"], true,
            "positive control: the stand-in provider is found through the sentinel PATH"
        );
        let host = addr.to_string();
        let request = format!("GET /floor/release-manifest.json HTTP/1.1\r\nHost: {host}\r\n\r\n");
        let (head, body) = raw_onboard(addr, request.as_bytes());
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
        format!(
            "{}\n{request}{head}{}",
            String::from_utf8(bytes).expect("UTF-8 frame"),
            String::from_utf8(body).expect("UTF-8 manifest")
        )
    };

    let under_home = frames(home.join("state"), runtime);
    for sentinel in [
        "private-path-sentinel",
        "private-runtime-sentinel",
        "private-env-sentinel",
        "private-home-sentinel",
    ] {
        assert!(
            !under_home.contains(sentinel),
            "a surface exposed {sentinel}"
        );
    }
    let outside = frames(
        root.join("elsewhere").join("private-home-sentinel"),
        root.join("run"),
    );
    assert!(
        outside.contains("private-home-sentinel"),
        "positive control: a state directory outside the home is shown in full"
    );
}

#[test]
fn browser_daemon_view_requires_the_session() {
    let fixture = Fixture::new();
    let settings = with_floor(fixture.settings());
    let (_, mut running, addr, cookie, csrf) = default_floor(&settings);
    assert!(
        snapshot(addr, &cookie, &csrf).0["daemon"].is_object(),
        "positive control: this session's snapshot carries the sibling"
    );
    let host = addr.to_string();
    let origin = format!("http://{host}");
    let wrong_csrf = "A".repeat(43);
    for (label, host, origin, cookie, csrf) in [
        (
            "Host",
            "invalid",
            origin.as_str(),
            cookie.as_str(),
            csrf.as_str(),
        ),
        (
            "Origin",
            host.as_str(),
            "null",
            cookie.as_str(),
            csrf.as_str(),
        ),
        (
            "cookie",
            host.as_str(),
            origin.as_str(),
            "invalid",
            csrf.as_str(),
        ),
        (
            "CSRF",
            host.as_str(),
            origin.as_str(),
            cookie.as_str(),
            wrong_csrf.as_str(),
        ),
    ] {
        let head = upgrade_head(addr, host, origin, cookie, csrf);
        assert!(
            head.starts_with("HTTP/1.1 403 Forbidden"),
            "{label}: {head}"
        );
        assert!(!head.contains("daemon"), "{label}: nothing of the sibling");
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_daemon_check_is_bounded_by_its_deadline() {
    let fixture = Fixture::new();
    let settings = with_floor(fixture.settings());
    let (check, mut running, addr, cookie, csrf) = default_floor(&settings);
    check.hold_next();
    let started = std::time::Instant::now();
    let (held, _) = snapshot(addr, &cookie, &csrf);
    let waited = started.elapsed();
    assert!(check.wait_held(Duration::ZERO), "the check is still held");
    assert_eq!(
        held.get("daemon"),
        None,
        "a check past its deadline adds nothing"
    );
    assert!(
        waited < Duration::from_millis(1_500),
        "the snapshot waited {waited:?} for a held check"
    );

    let again = std::time::Instant::now();
    let (during, _) = snapshot(addr, &cookie, &csrf);
    assert!(
        again.elapsed() < Duration::from_millis(500),
        "a snapshot after the running check's deadline goes at once"
    );
    assert_eq!(during.get("daemon"), None);
    assert_eq!(check.runs(), 1, "no second check starts while one is held");

    check.release();
    std::thread::sleep(PAST_REUSE);
    let (after, _) = snapshot(addr, &cookie, &csrf);
    assert!(after["daemon"].is_object(), "a fresh check answers again");
    assert_eq!(
        after["projection"], held["projection"],
        "joining the sibling leaves the projection as it was"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_daemon_check_runs_at_most_once_per_second() {
    let fixture = Fixture::new();
    let settings = with_floor(fixture.settings());
    let (check, mut running, addr, cookie, csrf) = default_floor(&settings);
    let started = std::time::Instant::now();
    for _ in 0..50 {
        assert!(snapshot(addr, &cookie, &csrf).0["daemon"].is_object());
    }
    let elapsed = started.elapsed();
    let allowed = 1 + usize::try_from(elapsed.as_secs()).expect("seconds");
    assert!(
        check.runs() <= allowed,
        "{} checks for fifty subscriptions in {elapsed:?}",
        check.runs()
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_daemon_and_settings_siblings_join_on_the_connection_thread() {
    for enrolled in [false, true] {
        let fixture = Fixture::new();
        let settings = if enrolled {
            enrolled_settings(&fixture)
        } else {
            with_floor(fixture.settings())
        };
        let fault = duded::floor_stream::FloorStreamFault::default();
        let daemon = duded::boot::boot_with_floor_faults(
            &settings,
            seams(
                &settings,
                SharedHost::new(),
                Arc::new(ScriptedRunner::default()),
                Arc::new(TestClock::new()),
                Witness::default(),
            ),
            None,
            fault.clone(),
        )
        .expect("boot Floor");
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        for _ in 0..3 {
            let (frame, _) = snapshot(addr, &cookie, &csrf);
            assert!(frame["daemon"].is_object());
            assert!(frame["settings"].is_object());
            assert_eq!(frame["onboarding"].is_object(), enrolled);
            assert_eq!(
                fault.settings_joined_on().as_deref(),
                Some("dudes-floor-connection"),
                "enrolled {enrolled}: the settings sibling joins off the loop thread"
            );
        }
        assert_eq!(
            fault.subscription_serializations(),
            3,
            "enrolled {enrolled}: the loop thread serializes only for its own siblings"
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

#[test]
fn browser_daemon_check_runs_off_the_loop_thread() {
    let fixture = Fixture::new();
    let settings = with_floor(fixture.settings());
    let (check, mut running, addr, cookie, csrf) = default_floor(&settings);
    let workspace = fixture.state.path().join("repo");
    std::fs::create_dir_all(&workspace).expect("a workspace");
    let agent = common::hire(&running.handle, &workspace);
    check.hold_next();
    let subscriber = std::thread::spawn(move || snapshot_optional(addr, &cookie, &csrf));
    assert!(
        check.wait_held(Duration::from_secs(5)),
        "the subscription started a check"
    );
    let handle = running.handle.clone();
    let pause_call = std::thread::spawn(move || handle.ask(duded::ports::Ask::Pause(agent)));
    let paused = journaled_within(
        &fixture,
        Duration::from_secs(3),
        |record| matches!(record, Record::Pause { agent: named, .. } if *named == agent),
    );
    let still_held = check.wait_held(Duration::ZERO);
    check.release();
    assert!(paused, "a local pause is journaled while the check is held");
    assert!(still_held, "the check was held throughout");
    pause_call
        .join()
        .expect("the pause thread")
        .expect("the pause");
    subscriber.join().expect("the subscription thread");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

const RETIRE: &str = "/floor/retire";

/// Floor settings with pause and resume on, and retiring on or off. Nothing else differs from the
/// fixture's.
fn retire_settings(fixture: &Fixture, enabled: bool) -> duded::Settings {
    let mut settings = control_settings(fixture, true);
    settings.floor_control = FloorControl::On {
        goal: false,
        retire: enabled,
        renew: false,
    };
    settings
}

fn retire_body(request_id: &str, agent: AgentId) -> Value {
    json!({ "request_id": request_id, "agent": agent })
}

fn post_retire(addr: SocketAddr, cookie: &str, csrf: &str, body: &Value) -> (String, Value) {
    post_bytes(
        addr,
        RETIRE,
        cookie,
        csrf,
        &serde_json::to_vec(body).expect("retire JSON"),
        None,
    )
}

/// A `200 retired`; returns the retirement's position.
fn retired_at(head: &str, response: &Value) -> Lsn {
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {response}");
    assert!(head.contains("Cache-Control: no-store"), "{head}");
    assert_eq!(response["state"], "retired", "{response}");
    Lsn(response["retire_lsn"]
        .as_str()
        .expect("a decimal string")
        .parse()
        .expect("a u64"))
}

/// Every `AgentRetired` naming `agent`: its position, reason and attribution.
fn retirements(fixture: &Fixture, agent: AgentId) -> Vec<(Lsn, String, Option<BrowserRetire>)> {
    fixture
        .entries()
        .into_iter()
        .filter_map(|(lsn, entry)| match entry.record {
            Record::AgentRetired {
                id,
                reason,
                browser_retire,
            } if id == agent => Some((lsn, reason, browser_retire)),
            _ => None,
        })
        .collect()
}

fn retired_count(records: &[Record]) -> usize {
    records
        .iter()
        .filter(|record| matches!(record, Record::AgentRetired { .. }))
        .count()
}

/// The request digest, recomputed here rather than taken from the daemon.
fn expected_retire_digest(agent: AgentId) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let agent = agent.to_string();
    let mut hasher = Sha256::new();
    hasher.update(b"dudes.retire.v1\0");
    hasher.update(5_u16.to_be_bytes());
    hasher.update(b"agent");
    hasher.update(u32::try_from(agent.len()).unwrap().to_be_bytes());
    hasher.update(agent.as_bytes());
    hasher.finalize().into()
}

/// A hired agent with a goal whose first turn is spawned on the fake host.
fn working(running: &common::Running, fixture: &Fixture) -> (AgentId, TurnId) {
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[true_program()]),
    );
    let turn = fixture.await_record("the turn spawned", |record| match record {
        Record::TurnSpawned {
            agent: who, turn, ..
        } if *who == agent => Some(*turn),
        _ => None,
    });
    (agent, turn)
}

/// The primary test, stub-backed and read from the replayed journal, in its seven
/// ordered steps: a browser retire of a locally hired agent with a running turn applies at R, is
/// attributed to its session, kills that turn, issues nothing after R, withdraws the agent's token
/// from outside its children, removes the agent from the Floor, and replays to the same truth with
/// the index answering the key.
///
/// Mutant: boot replays no retire index, and step 7's retry is refused `agent_absent`.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end stub scenario asserts its seven ordered steps on one journal"
)]
fn browser_retire_is_finite_attributed_and_ends_the_agent() {
    let fixture = Fixture::new();
    let capture = tempfile::tempdir().expect("task capture directory");
    let mut settings = retire_settings(&fixture, true);
    settings.daemon_env.push(EnvVar::new(HELPER_ROLE, "1"));
    settings
        .daemon_env
        .push(EnvVar::new(TASK_CAPTURE, capture.path().as_os_str()));
    settings.daemon_env.push(EnvVar::new(
        STUB_HOLD,
        capture.path().join("never-released").as_os_str(),
    ));
    let spawns = || {
        std::fs::read_dir(capture.path())
            .expect("capture directory")
            .filter(|file| {
                file.as_ref()
                    .is_ok_and(|file| file.path().extension().is_some_and(|ext| ext == "task"))
            })
            .count()
    };
    let parts = real_stub_seams(&settings);
    let tokens = parts.rng.tokens();
    let daemon = boot(&settings, parts).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[true_program()]),
    );
    let t1 = fixture.await_record("T1 spawned", |record| match record {
        Record::TurnSpawned {
            agent: who, turn, ..
        } if *who == agent => Some(*turn),
        _ => None,
    });
    let token = common::hook_shim::minted_token(&fixture, &tokens, agent);
    let ping = || {
        dudes_rpc::Client::connect(settings.socket())
            .expect("connect")
            .call(Method::SystemPing, Some(json!({ "token": token.expose() })))
            .expect("answered")
    };
    let before = ping()
        .error
        .expect("an outside peer may not ping as the agent");
    assert_eq!(
        before.code,
        dudes_proto::rpc::ErrorCode::Unauthorized.code(),
        "control: before the retirement the token still names a live agent: {before:?}"
    );
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (view, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(view["retire"], json!({}), "retiring is enabled");
    let mut stream = OpenStream::open(addr, &cookie, &csrf);

    // Step 1: T1 is running; a browser retire presenting A applies at R.
    let wire = request_id(1);
    let (head, response) = post_retire(addr, &cookie, &csrf, &retire_body(&wire, agent));
    let r = retired_at(&head, &response);
    assert_eq!(
        response,
        json!({
            "request_id": wire, "state": "retired", "agent": agent,
            "retire_lsn": r.0.to_string(),
        })
    );

    // Step 5: from outside A's recorded children, A's token is refused as a token.
    let after = ping();
    let error = after.error.expect("the retired agent's token is refused");
    assert_eq!(error.code, dudes_proto::rpc::ErrorCode::Refused.code());
    let data: dudes_proto::rpc::RefusedData =
        serde_json::from_value(error.data.expect("typed refusal")).expect("RefusedData");
    assert_eq!(data.gate, dudes_proto::rpc::RefusalGate::Token);

    // Step 6: a delta names A in `removed_agents`, and a fresh snapshot has no row for A.
    let removed = loop {
        let frame = stream
            .next_frame(Duration::from_secs(5))
            .expect("a delta after the retirement");
        let removed = &frame["delta"]["removed_agents"];
        if frame["type"] == "floor.delta" && removed.as_array().is_some_and(|ids| !ids.is_empty()) {
            break removed.clone();
        }
    };
    assert_eq!(removed, json!([agent]));
    let (view, _) = snapshot(addr, &cookie, &csrf);
    assert!(
        !view["projection"]["agents"]
            .as_array()
            .expect("agents")
            .iter()
            .any(|row| row["id"] == json!(agent)),
        "{view}"
    );

    // Step 3: T1 ends killed by the retirement, and its predicate result decides nothing.
    let ended = fixture.await_record("T1's end", |record| match record {
        Record::TurnEnded { turn, outcome, .. } if *turn == t1 => Some(outcome.clone()),
        _ => None,
    });
    assert_eq!(
        ended,
        Some(TurnOutcome::Killed {
            by: dudes_proto::KillReason::Retire
        })
    );
    fixture.await_record("T1's predicate result", |record| match record {
        Record::PredicateResult {
            turn: Some(turn), ..
        } if *turn == t1 => Some(()),
        _ => None,
    });
    // A job through the same writer, so every effect of that pump has run.
    let _ = snapshot(addr, &cookie, &csrf);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    // Step 2: the attributed record at R names the admitting session.
    let entries = fixture.entries();
    let Some(Record::AgentRetired {
        id,
        reason,
        browser_retire: Some(attribution),
    }) = entries
        .iter()
        .find(|(at, _)| *at == r)
        .map(|(_, entry)| entry.record.clone())
    else {
        panic!("an attributed AgentRetired at R");
    };
    assert_eq!((id, reason.as_str()), (agent, "retired from the Floor"));
    let mut first_id = [0_u8; 32];
    first_id[0] = 4;
    assert_eq!(attribution.request_id.bytes(), first_id);
    assert_eq!(
        attribution.request_digest.bytes(),
        expected_retire_digest(agent)
    );
    assert_eq!(
        attribution.session_digest.bytes(),
        <[u8; 32]>::from(<sha2::Sha256 as sha2::Digest>::digest(cookie.as_bytes()))
    );
    assert_eq!(retirements(&fixture, agent).len(), 1);
    let ended_at = entries
        .iter()
        .find_map(|(lsn, entry)| {
            matches!(&entry.record, Record::TurnEnded { turn, .. } if *turn == t1).then_some(*lsn)
        })
        .expect("T1's end");
    assert!(ended_at > r, "the kill follows the retirement");

    // Step 4: nothing is issued after R, and the stub saw one launch.
    assert!(
        !entries.iter().any(|(lsn, entry)| *lsn > r
            && matches!(&entry.record, Record::TurnIssued { agent: who, .. } if *who == agent)),
        "no TurnIssued for A after R"
    );
    assert_eq!(spawns(), 1, "no further stub spawn");
    let journal = serde_json::to_string(&entries).expect("journal JSON");
    for secret in [cookie.as_str(), csrf.as_str(), wire.as_str()] {
        assert!(
            !journal.contains(secret),
            "raw browser credential in the journal"
        );
    }

    // Step 7: replay keeps A retired, the index answers the key, and nothing is issued or spawned.
    let host = SharedHost::new();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("replay boot");
    assert!(daemon.world().agents[&agent].retired);
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    let (head, replayed) = post_retire(addr, &cookie, &csrf, &retire_body(&wire, agent));
    assert_eq!(retired_at(&head, &replayed), r);
    assert_eq!(replayed, response, "the original 200");
    let _ = snapshot(addr, &cookie, &csrf);
    assert_eq!(fixture.records(), before, "nothing appended after replay");
    assert!(host.spawned().is_empty(), "nothing spawned after replay");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Every browser credential is checked before any effect: a wrong Host, a wrong or missing Origin,
/// an unknown cookie, a wrong or missing CSRF and an expired session each get the uniform empty
/// `403`, and the journal does not move. Control: the right credentials apply.
#[test]
fn browser_retire_refuses_wrong_session_host_origin_and_csrf_without_effect() {
    let fixture = Fixture::new();
    let settings = retire_settings(&fixture, true);
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = serde_json::to_vec(&retire_body(&request_id(1), agent)).unwrap();
    let host = addr.to_string();
    let request = |host: &str, origin: Option<&str>, cookie: &str, csrf: Option<&str>| {
        let mut head = format!("POST {RETIRE} HTTP/1.1\r\nHost: {host}\r\n");
        if let Some(origin) = origin {
            let _ = write!(head, "Origin: {origin}\r\n");
        }
        let _ = write!(head, "Cookie: dudes_floor_session={cookie}\r\n");
        if let Some(csrf) = csrf {
            let _ = write!(head, "X-Dudes-CSRF: {csrf}\r\n");
        }
        let _ = write!(
            head,
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&body);
        bytes
    };
    let origin = format!("http://{host}");
    let unknown = "A".repeat(43);
    let before = fixture.records();
    for (label, bytes) in [
        (
            "wrong Host",
            request("127.0.0.1:1", Some(&origin), &cookie, Some(&csrf)),
        ),
        (
            "wrong Origin",
            request(&host, Some("http://127.0.0.1:1"), &cookie, Some(&csrf)),
        ),
        ("missing Origin", request(&host, None, &cookie, Some(&csrf))),
        (
            "unknown cookie",
            request(&host, Some(&origin), &unknown, Some(&csrf)),
        ),
        (
            "wrong CSRF",
            request(&host, Some(&origin), &cookie, Some(&unknown)),
        ),
        ("missing CSRF", request(&host, Some(&origin), &cookie, None)),
    ] {
        let (head, response) = raw_onboard(addr, &bytes);
        assert!(
            head.starts_with("HTTP/1.1 403 Forbidden"),
            "{label}: {head}"
        );
        assert!(response.is_empty(), "{label}: no envelope");
    }
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let (expired_cookie, expired_csrf) = exchange(addr, &issue(&settings).bootstrap);
    clock.advance(30 * 60 * 1000);
    let (fresh_cookie, fresh_csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, response) = post_retire(
        addr,
        &expired_cookie,
        &expired_csrf,
        &retire_body(&request_id(1), agent),
    );
    empty_refusal(&head, &response, "HTTP/1.1 403 Forbidden");
    assert_eq!(
        fixture.records(),
        before,
        "an expired session appends nothing"
    );
    let _ = (cookie, csrf);

    let (head, response) = post_retire(
        addr,
        &fresh_cookie,
        &fresh_csrf,
        &retire_body(&request_id(1), agent),
    );
    retired_at(&head, &response);
    assert_eq!(retirements(&fixture, agent).len(), 1);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The refusal reasons, the first that holds, each with no record and no kill: an unknown and an already
/// retired agent are `agent_absent`; a locally halted agent is `agent_halted`, even when its turn's
/// liveness is also unknown; an agent whose open turn's liveness is `Unknown` is
/// `liveness_unknown`. Control: a live, unhalted agent whose turn is `Alive` applies.
///
/// Mutant: drop the liveness check, and the `Unknown` arm appends an `AgentRetired`.
#[test]
fn browser_retire_refuses_absent_halted_and_unknown_liveness() {
    let fixture = Fixture::new();
    let settings = retire_settings(&fixture, true);
    let host = SharedHost::new();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let retired = common::hire(&running.handle, fixture.state.path());
    running
        .handle
        .ask(Ask::Retire(Box::new(RetireParams {
            agent: retired,
            reason: "locally".into(),
        })))
        .expect("local retirement");
    let (halted, halted_turn) = working(&running, &fixture);
    running.handle.ask(Ask::Halt(halted)).expect("local halt");
    let (unknown, unknown_turn) = working(&running, &fixture);
    let (live, live_turn) = working(&running, &fixture);
    for (turn, liveness) in [
        (halted_turn, dudes_supervisor::host::Liveness::Unknown),
        (unknown_turn, dudes_supervisor::host::Liveness::Unknown),
        (live_turn, dudes_supervisor::host::Liveness::Alive),
    ] {
        running.handle.input(Input::Liveness { turn, liveness });
    }
    running.handle.doctor().expect("order the liveness inputs");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    let kills = || host.peek(|host| host.killed.len());
    let kills_before = kills();
    for (k, agent, reason) in [
        (1, AgentId::from_parts(9, 9), "agent_absent"),
        (2, retired, "agent_absent"),
        (3, halted, "agent_halted"),
        (4, unknown, "liveness_unknown"),
    ] {
        let (head, response) =
            post_retire(addr, &cookie, &csrf, &retire_body(&request_id(k), agent));
        refused(&head, &response, reason);
        assert_eq!(response["request_id"], request_id(k), "{response}");
    }
    assert_eq!(fixture.records(), before, "no refusal appended anything");
    assert_eq!(kills(), kills_before, "no refusal killed anything");

    let (head, response) = post_retire(addr, &cookie, &csrf, &retire_body(&request_id(5), live));
    retired_at(&head, &response);
    assert_eq!(retirements(&fixture, live).len(), 1, "control: it applied");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Every applied body is single use. Resent under a new identity it is `agent_absent`; resent
/// under its own it gets its original `200`. No new record either way.
#[test]
fn browser_retire_replayed_body_has_no_effect() {
    let fixture = Fixture::new();
    let settings = retire_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = retire_body(&request_id(1), agent);
    let (head, original) = post_retire(addr, &cookie, &csrf, &body);
    retired_at(&head, &original);
    let applied_records = fixture.records();

    let mut replayed = body.clone();
    replayed["request_id"] = json!(request_id(2));
    let (head, response) = post_retire(addr, &cookie, &csrf, &replayed);
    refused(&head, &response, "agent_absent");
    let (head, response) = post_retire(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(response, original, "its own identity gets its original 200");
    assert_eq!(fixture.records(), applied_records, "zero new records");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The journal-derived index answers a same-key retry with its original result, from the same
/// session and from a fresh one after a restart, with no new record. The same key with a changed
/// agent is an empty `409`.
///
/// Mutant: the index answer ignores the digest, and the changed payload gets the original `200`.
#[test]
fn browser_retire_same_key_retry_returns_original_and_changed_payload_conflicts() {
    let fixture = Fixture::new();
    let settings = retire_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let other = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = retire_body(&request_id(1), agent);
    let (head, original) = post_retire(addr, &cookie, &csrf, &body);
    retired_at(&head, &original);
    let later = fixture.records();

    let (head, response) = post_retire(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(response, original);
    let mut changed = body.clone();
    changed["agent"] = json!(other);
    let (head, response) = post_retire(addr, &cookie, &csrf, &changed);
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    assert_eq!(fixture.records(), later, "no retry appended anything");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let daemon = boot(&settings, fake_seams(&settings)).expect("restart");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, response) = post_retire(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(response, original, "from a fresh session after a restart");
    let (head, response) = post_retire(addr, &cookie, &csrf, &changed);
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    assert_eq!(fixture.records(), later);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// While a request is in flight: with the owner past its world check and before its append
/// permit, a same-key follower gets `202 pending` and a changed payload an empty `409`, both
/// without the writer. Then the owner's `200`. Two keys sent from one page view yield one record
/// and one `agent_absent`.
#[test]
fn browser_retire_concurrent_same_key_is_pending_and_two_keys_yield_one_record() {
    let fixture = Fixture::new();
    let settings = retire_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let second = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = retire_body(&request_id(1), agent);

    pause.arm(OnboardingPausePoint::BeforeRetirePermit);
    let owner = {
        let (cookie, csrf, body) = (cookie.clone(), csrf.clone(), body.clone());
        std::thread::spawn(move || post_retire(addr, &cookie, &csrf, &body))
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the owner is in flight"
    );
    let at_once = |body: Value| {
        let (tx, rx) = mpsc::channel();
        let (cookie, csrf) = (cookie.clone(), csrf.clone());
        std::thread::spawn(move || {
            let _ = tx.send(post_retire(addr, &cookie, &csrf, &body));
        });
        rx.recv_timeout(Duration::from_secs(3)).ok()
    };
    let follower = at_once(body.clone());
    let mut changed = body.clone();
    changed["agent"] = json!(second);
    let changed = at_once(changed);
    pause.release();
    let (head, pending) = follower.expect("a same-key follower answers without the writer");
    assert!(head.starts_with("HTTP/1.1 202 Accepted"), "{head}");
    assert_eq!(
        pending,
        json!({ "request_id": request_id(1), "state": "pending" })
    );
    let (head, response) = changed.expect("a changed payload answers without the writer");
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    let (head, response) = owner.join().expect("owner");
    retired_at(&head, &response);
    assert_eq!(retirements(&fixture, agent).len(), 1);

    let (head, response) = post_retire(addr, &cookie, &csrf, &retire_body(&request_id(2), second));
    retired_at(&head, &response);
    let (head, response) = post_retire(addr, &cookie, &csrf, &retire_body(&request_id(3), second));
    refused(&head, &response, "agent_absent");
    assert_eq!(retirements(&fixture, second).len(), 1, "one record");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Consequence of the single writer: a local command that reaches the writer first is never repeated. A local
/// retirement before the request gives `agent_absent`, a local halt `agent_halted`; and a request
/// whose append failed before its fsync, then a local retirement, then a same-key retry gives
/// `agent_absent`.
#[test]
fn browser_retire_local_winner_is_never_repeated() {
    let fixture = Fixture::new();
    let settings = retire_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let append_failure = daemon.append_failure();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let retired = common::hire(&running.handle, fixture.state.path());
    let halted = common::hire(&running.handle, fixture.state.path());
    let crashed = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let local_retire = |agent| {
        running
            .handle
            .ask(Ask::Retire(Box::new(RetireParams {
                agent,
                reason: "locally".into(),
            })))
            .expect("local retirement");
    };
    local_retire(retired);
    running.handle.ask(Ask::Halt(halted)).expect("local halt");
    let before = fixture.records();
    for (k, agent, reason) in [(1, retired, "agent_absent"), (2, halted, "agent_halted")] {
        let (head, response) =
            post_retire(addr, &cookie, &csrf, &retire_body(&request_id(k), agent));
        refused(&head, &response, reason);
    }
    assert_eq!(fixture.records(), before, "no local record was repeated");

    append_failure.fail_nth(NonZeroUsize::new(1).unwrap());
    let body = retire_body(&request_id(3), crashed);
    let (head, response) = post_retire(addr, &cookie, &csrf, &body);
    empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
    assert!(retirements(&fixture, crashed).is_empty());
    local_retire(crashed);
    let (head, response) = post_retire(addr, &cookie, &csrf, &body);
    refused(&head, &response, "agent_absent");
    let records = retirements(&fixture, crashed);
    assert_eq!(records.len(), 1, "only the local record");
    assert!(
        records[0].2.is_none(),
        "the local record carries no attribution"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Across restarts, at two cuts: after the retirement's fsync and before its fold, and after
/// the fold and the kill and before the killed turn's `TurnEnded`. Each replays exactly one
/// `AgentRetired`, issues no turn for the agent in either run, and answers the same key from the
/// index. The first cut signals no kill in its first run; the second signals exactly one.
#[test]
fn browser_retire_restart_cut_points_replay_one_record() {
    for fold_first in [false, true] {
        let fixture = Fixture::new();
        let settings = retire_settings(&fixture, true);
        let host = SharedHost::new();
        let run = |host: &SharedHost| {
            boot(
                &settings,
                seams(
                    &settings,
                    host.clone(),
                    Arc::new(ScriptedRunner::default()),
                    Arc::new(TestClock::new()),
                    Witness::default(),
                ),
            )
            .expect("boot Floor")
        };
        let daemon = run(&host);
        let failure = daemon.onboarding_failure();
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (agent, _) = working(&running, &fixture);
        let other = common::hire(&running.handle, fixture.state.path());
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let body = retire_body(&request_id(1), agent);
        if !fold_first {
            failure.arm(OnboardingFailurePoint::Retire);
        }
        let issued_before = fixture
            .records()
            .iter()
            .filter(
                |record| matches!(record, Record::TurnIssued { agent: who, .. } if *who == agent),
            )
            .count();
        let (head, response) = post_retire(addr, &cookie, &csrf, &body);
        if fold_first {
            retired_at(&head, &response);
        } else {
            empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
            let (head, pending) = post_retire(addr, &cookie, &csrf, &body);
            assert!(head.starts_with("HTTP/1.1 202 Accepted"), "{head}");
            assert_eq!(
                pending["state"], "pending",
                "an uncertain append keeps its key"
            );
            let (head, response) =
                post_retire(addr, &cookie, &csrf, &retire_body(&request_id(2), other));
            empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
        }
        let kills = host.peek(|host| host.killed.len());
        assert_eq!(
            kills,
            usize::from(fold_first),
            "fold first {fold_first}: kills"
        );
        assert!(
            !fixture.records().iter().any(|record| matches!(
                record,
                Record::TurnEnded { agent: who, .. } if *who == agent
            )),
            "the cut is before the killed turn's end"
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
        if !fold_first {
            // The harness stops cleanly and sweeps its fake child; the cut models a crash where
            // that child survives for the next run to re-adopt and kill.
            let (pid, start) = host.peek(|host| (host.spawned_pids[0], host.next_start_time));
            host.answer(pid, dudes_supervisor::host::ProbeAnswer::StartValue(start));
        }

        let daemon = run(&host);
        assert!(daemon.world().agents[&agent].retired, "replay folds it");
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let (head, response) = post_retire(addr, &cookie, &csrf, &body);
        let r = retired_at(&head, &response);
        let records = retirements(&fixture, agent);
        assert_eq!(records.len(), 1, "fold first {fold_first}: one record");
        assert_eq!(records[0].0, r);
        assert!(retirements(&fixture, other).is_empty());
        let issued = fixture
            .records()
            .iter()
            .filter(
                |record| matches!(record, Record::TurnIssued { agent: who, .. } if *who == agent),
            )
            .count();
        assert_eq!(
            issued, issued_before,
            "fold first {fold_first}: no turn issued"
        );
        assert_eq!(
            host.peek(|host| host.killed.len()),
            1,
            "fold first {fold_first}: one kill across both runs"
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

/// No clock read happens under the registry lock: session A holds its retirement's append
/// permit, and so the registry lock, while session B's retirement waits for admission and the test
/// clock crosses the absolute expiry. A's held permit wins; B gets the uniform empty `403` and
/// appends nothing; and no clock read happens while A holds the lock.
#[test]
fn browser_retire_expiry_waiting_on_registry_lock_is_forbidden() {
    let fixture = Fixture::new();
    let settings = retire_settings(&fixture, true);
    let clock = Arc::new(ProbeClock {
        inner: TestClock::ticking(1_800_000_000_000, 0),
        armed: Mutex::new(None),
    });
    let daemon = boot(
        &settings,
        Seams {
            host: SharedHost::new(),
            pty_host: Box::new(duded::ports::ClosedPtyHost),
            predicates: Arc::new(ScriptedRunner::default()),
            clock: Arc::clone(&clock),
            rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
            resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
            adapters: [Box::new(StubAdapter) as Box<dyn Adapter>]
                .into_iter()
                .collect(),
            children: ChildEnds::new(),
            witness: Witness::default(),
        },
    )
    .expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let a = common::hire(&running.handle, fixture.state.path());
    let b = common::hire(&running.handle, fixture.state.path());
    let (a_cookie, a_csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (b_cookie, b_csrf) = exchange(addr, &issue(&settings).bootstrap);

    pause.arm(OnboardingPausePoint::AfterRetirePermit);
    let holder = std::thread::spawn(move || {
        post_retire(addr, &a_cookie, &a_csrf, &retire_body(&request_id(1), a))
    });
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "A holds its append permit"
    );
    let probe = clock.arm();
    let (sent_tx, sent_rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let body = serde_json::to_vec(&retire_body(&request_id(2), b)).unwrap();
        post_bytes(addr, RETIRE, &b_cookie, &b_csrf, &body, Some(sent_tx))
    });
    sent_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("B is sent while A holds the lock");
    let sampled_while_held = probe.recv_timeout(Duration::from_millis(500)).is_ok();
    clock.inner.advance(8 * 60 * 60 * 1000);
    pause.release();
    let (head, winner) = holder.join().expect("A");
    retired_at(&head, &winner);
    let (head, response) = waiter.join().expect("B");
    assert!(
        probe.recv_timeout(Duration::from_secs(2)).is_ok() || sampled_while_held,
        "positive control: the clock was read once armed"
    );
    assert!(
        !sampled_while_held,
        "session validity was sampled while another session held the registry lock"
    );
    empty_refusal(&head, &response, "HTTP/1.1 403 Forbidden");
    assert_eq!(retirements(&fixture, a).len(), 1);
    assert!(
        retirements(&fixture, b).is_empty(),
        "the expired waiter appended nothing"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// An uncertain write fences the daemon. A retirement whose append fails after its fsync gets an
/// empty `503`, keeps its key pending, and leaves the agent live in this run; a fresh request gets
/// an empty `503` and appends nothing, and a durable retirement's same-key retry is still served
/// from the index. After a restart that replays the fenced retirement, the index answers its key.
#[test]
fn browser_retire_uncertain_local_write_fences_fresh_requests_and_serves_durable_retries() {
    let fixture = Fixture::new();
    let settings = retire_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let failure = daemon.onboarding_failure();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let durable_agent = common::hire(&running.handle, fixture.state.path());
    let fenced_agent = common::hire(&running.handle, fixture.state.path());
    let fresh_agent = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let durable = retire_body(&request_id(1), durable_agent);
    let (head, original) = post_retire(addr, &cookie, &csrf, &durable);
    retired_at(&head, &original);

    failure.arm(OnboardingFailurePoint::Retire);
    let fenced = retire_body(&request_id(2), fenced_agent);
    let (head, response) = post_retire(addr, &cookie, &csrf, &fenced);
    empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
    assert!(
        snapshot_optional(addr, &cookie, &csrf).is_none(),
        "unresolved retirement cannot publish a normal snapshot"
    );
    let on_disk = fixture.records();
    assert_eq!(
        retirements(&fixture, fenced_agent).len(),
        1,
        "it is durable"
    );

    let (head, retry) = post_retire(addr, &cookie, &csrf, &durable);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "while fenced: {head}");
    assert_eq!(retry, original);
    for _ in 0..2 {
        let (head, response) = post_retire(
            addr,
            &cookie,
            &csrf,
            &retire_body(&request_id(3), fresh_agent),
        );
        empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
    }
    assert_eq!(fixture.records(), on_disk, "nothing appended while fenced");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let daemon = boot(&settings, fake_seams(&settings)).expect("restart");
    assert!(daemon.world().agents[&fenced_agent].retired);
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, response) = post_retire(addr, &cookie, &csrf, &fenced);
    retired_at(&head, &response);
    assert_eq!(retirements(&fixture, fenced_agent).len(), 1);
    assert!(retirements(&fixture, fresh_agent).is_empty());
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The request bounds refuse before any effect: 513 bytes (`413`, before a byte is read); a `reason`, a
/// `name` and an `action` member; a duplicate, a nested and a missing member; a noncanonical
/// request identity or agent; an array. Control: the 97-byte canonical body applies.
///
/// Mutant: drop `deny_unknown_fields`, and the extra members apply.
#[test]
fn browser_retire_bounds_and_json_shape_refuse_before_effect() {
    let fixture = Fixture::new();
    let settings = retire_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let id = request_id(1);
    let alias = agent.to_string().replacen('0', "O", 1);
    assert_ne!(alias, agent.to_string(), "the alias spelling differs");
    let refusals: Vec<(&str, String)> = vec![
        (
            "reason member",
            format!(r#"{{"request_id":"{id}","agent":"{agent}","reason":"x"}}"#),
        ),
        (
            "name member",
            format!(r#"{{"request_id":"{id}","agent":"{agent}","name":"x"}}"#),
        ),
        (
            "action member",
            format!(r#"{{"request_id":"{id}","agent":"{agent}","action":"halt"}}"#),
        ),
        (
            "duplicate member",
            format!(r#"{{"request_id":"{id}","agent":"{agent}","agent":"{agent}"}}"#),
        ),
        (
            "nested member",
            format!(r#"{{"request_id":"{id}","agent":{{"id":"{agent}"}}}}"#),
        ),
        ("missing agent", format!(r#"{{"request_id":"{id}"}}"#)),
        (
            "noncanonical request_id",
            format!(
                r#"{{"request_id":"{}B","agent":"{agent}"}}"#,
                "A".repeat(42)
            ),
        ),
        (
            "alias-letter agent",
            format!(r#"{{"request_id":"{id}","agent":"{alias}"}}"#),
        ),
        ("an array", format!(r#"["{id}","{agent}"]"#)),
    ];
    let before = fixture.records();
    for (label, body) in &refusals {
        let (head, response) = post_bytes(addr, RETIRE, &cookie, &csrf, body.as_bytes(), None);
        assert!(
            head.starts_with("HTTP/1.1 400 Bad Request"),
            "{label}: {head}"
        );
        assert!(response.is_null(), "{label}: {response}");
    }
    let host = addr.to_string();
    let oversize = format!(
        "POST {RETIRE} HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: dudes_floor_session={cookie}\r\nX-Dudes-CSRF: {csrf}\r\nContent-Type: application/json\r\nContent-Length: 513\r\n\r\n"
    );
    let (head, body) = raw_onboard(addr, oversize.as_bytes());
    assert!(
        head.starts_with("HTTP/1.1 413"),
        "refused before a byte is read: {head}"
    );
    assert!(body.is_empty());
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let canonical = serde_json::to_string(&retire_body(&id, agent)).unwrap();
    assert_eq!(canonical.len(), 97);
    let (head, response) = post_bytes(addr, RETIRE, &cookie, &csrf, canonical.as_bytes(), None);
    retired_at(&head, &response);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// With retiring off, which is the default, the route does not exist: its response is byte for
/// byte an unrouted POST's, and no snapshot carries the `retire` sibling. That holds with control
/// off and with control on. Control: retiring on routes it and sends the sibling. And the
/// operator's switch loads with no budget, and is refused at load without `[floor.control]`.
///
/// Mutant: the route ignores its switch, and the control-on arm routes it.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "three switch settings and every load verdict share one table"
)]
fn browser_retire_is_absent_when_not_enabled() {
    for control in [
        FloorControl::Off,
        FloorControl::On {
            goal: true,
            retire: false,
            renew: false,
        },
        FloorControl::On {
            goal: false,
            retire: true,
            renew: false,
        },
    ] {
        let fixture = Fixture::new();
        let mut settings = retire_settings(&fixture, false);
        settings.floor_control = control;
        let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let agent = common::hire(&running.handle, fixture.state.path());
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let (view, _) = snapshot(addr, &cookie, &csrf);
        let body = serde_json::to_vec(&retire_body(&request_id(1), agent)).unwrap();
        let unrouted = raw_post(addr, "/floor/unrouted", &cookie, &csrf, &body);
        assert!(!unrouted.is_empty());
        let routed = raw_post(addr, RETIRE, &cookie, &csrf, &body);
        if control.retire() {
            assert!(
                String::from_utf8_lossy(&routed).starts_with("HTTP/1.1 200 OK"),
                "control: {}",
                String::from_utf8_lossy(&routed)
            );
            assert_eq!(view["retire"], json!({}));
            assert_eq!(retirements(&fixture, agent).len(), 1);
        } else {
            assert_eq!(
                routed,
                unrouted,
                "{control:?}: disabled is exactly unrouted: {}",
                String::from_utf8_lossy(&routed)
            );
            assert!(view.get("retire").is_none(), "{view}");
            assert_eq!(retired_count(&fixture.records()), 0);
        }
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }

    let root = tempfile::tempdir().expect("operator config root");
    let config = root.path().join("config.toml");
    let valid = "[budget]\nfloor_usd_per_day = 10\n[budget.per_turn_floor]\nusd = 0.01\n";
    let load = |source: String| {
        std::fs::write(&config, source).expect("config");
        load_operator(&config).map(|operator| operator.control)
    };
    assert_eq!(
        load(format!(
            "{valid}[floor.control]\nenabled = true\n[floor.retire]\nenabled = true\n"
        ))
        .expect("control: both switches with a ceiling and a reserve"),
        FloorControl::On {
            goal: false,
            retire: true,
            renew: false,
        }
    );
    assert_eq!(
        load(format!(
            "{valid}[floor.control]\nenabled = true\n[floor.goal]\nenabled = true\n[floor.retire]\nenabled = true\n"
        ))
        .expect("every switch"),
        FloorControl::On {
            goal: true,
            retire: true,
            renew: false,
        }
    );
    assert_eq!(
        load(format!(
            "{valid}[floor.control]\nenabled = true\n[floor.retire]\nenabled = false\n"
        ))
        .expect("retire off"),
        FloorControl::On {
            goal: false,
            retire: false,
            renew: false,
        }
    );
    assert_eq!(
        load("[floor.control]\nenabled = true\n[floor.retire]\nenabled = true\n".to_owned())
            .expect("retire needs no budget"),
        FloorControl::On {
            goal: false,
            retire: true,
            renew: false,
        }
    );
    for (label, source) in [
        (
            "no [floor.control]",
            format!("{valid}[floor.retire]\nenabled = true\n"),
        ),
        (
            "[floor.control] off",
            format!("{valid}[floor.control]\nenabled = false\n[floor.retire]\nenabled = true\n"),
        ),
        (
            "an unknown key",
            format!(
                "{valid}[floor.control]\nenabled = true\n[floor.retire]\nenabled = true\nreason = \"x\"\n"
            ),
        ),
    ] {
        assert!(load(source).is_err(), "{label} is refused at load");
    }
}

#[test]
fn browser_hire_unknown_appearance_is_refused_before_append() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
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
    .expect("boot Floor");
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let request = json!({"attempt_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "workspace_id":"repo_1", "name":"Ada", "role":"engineer", "goal_text":"ship"});
    let before = fixture.records();
    for (field, value) in [
        ("character", "a"),
        ("character", ""),
        ("character", "../cast"),
        ("accent", "red"),
        ("accent", "#fff"),
        ("accent", ""),
    ] {
        let mut unknown = request.clone();
        unknown[field] = json!(value);
        let (head, body) = post_onboard(addr, &cookie, &csrf, &unknown);
        assert!(
            head.starts_with("HTTP/1.1 400 Bad Request"),
            "{field}: {head}"
        );
        assert!(body.is_null());
        assert_eq!(
            fixture.records(),
            before,
            "unknown appearance appended a record"
        );
    }
    let mut chosen = request;
    chosen["character"] = json!("h");
    chosen["accent"] = json!("sky");
    let (head, _) = post_onboard(addr, &cookie, &csrf, &chosen);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(fixture.records().iter().any(|record| matches!(
        record,
        Record::AgentHired {
            character: Some(dudes_types::CastCharacter::H),
            accent: Some(dudes_types::AccentColour::Sky),
            ..
        }
    )));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

fn rename_body(id: &str, agent: AgentId, name: &str) -> Value {
    json!({"request_id": id, "agent": agent, "name": name})
}

fn post_rename(addr: SocketAddr, cookie: &str, csrf: &str, body: &Value) -> (String, Value) {
    post_bytes(
        addr,
        "/floor/rename",
        cookie,
        csrf,
        &serde_json::to_vec(body).unwrap(),
        None,
    )
}

fn post_look(addr: SocketAddr, cookie: &str, csrf: &str, body: &Value) -> (String, Value) {
    post_bytes(
        addr,
        "/floor/look",
        cookie,
        csrf,
        &serde_json::to_vec(body).unwrap(),
        None,
    )
}

fn seeded_look_hire(id: AgentId) -> Record {
    Record::AgentHired {
        id,
        name: "Kim".to_owned(),
        character: None,
        accent: None,
        role: "backend engineer".to_owned(),
        department: "engineering".to_owned(),
        provider: Provider::Claude,
        cwd: PathBuf::from("/"),
        workspace: None,
        budget: AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        trusted: false,
        worktree: None,
        mcp_servers: Vec::new(),
        browser_onboarding: None,
    }
}

fn seeded_look_card() -> Record {
    Record::CardCreated {
        card: dudes_types::CardId::from_parts(1, 9),
        title: String::new(),
        department: "engineering".to_owned(),
        by: dudes_proto::Actor::Peer {
            pid: 77,
            uid: Some(501),
        },
        deps: Vec::new(),
    }
}

fn look_title_room(mut candidate: duded::floor::AgentSnapshot) -> usize {
    for row in &mut candidate.agents {
        row.live_activity = Some(dudes_proto::LiveActivityView {
            kind: dudes_proto::LiveActivityKind::UsingTool,
            at: dudes_types::Timestamp::from_unix_millis(i64::MIN),
            expires_in_ms: 60_000,
        });
    }
    for row in &mut candidate.cards {
        row.status = dudes_types::CardStatus::Claimed;
        row.status_by = dudes_proto::Actor::Agent {
            agent: AgentId::NIL,
        };
        row.seq = dudes_types::Seq(u64::MAX);
        row.claimed_by = Some(AgentId::NIL);
    }
    let projection = dudes_proto::FloorProjection {
        lsn: Lsn(u64::MAX),
        agents: candidate.agents,
        cards: candidate.cards,
        mail: Vec::new(),
        activity: Vec::new(),
        retired_agents: Vec::new(),
        closing_time: None,
    };
    let baseline = serde_json::to_vec(&json!({
        "type":"floor.snapshot", "projection":projection,
        "messages":[], "messages_lsn":u64::MAX.to_string(), "threads":[], "threads_lsn":u64::MAX.to_string()
    }))
    .unwrap()
    .len();
    let budget = 1_048_576
        - 65_536
        - 65_536
        - dudes_proto::MAX_ACTIVITY_BYTES
        - dudes_proto::MAX_RETIRED_AGENT_BYTES
        - b",\"retired_agents\":".len()
        - dudes_proto::MAX_QUESTION_HISTORY_BYTES
        - b",\"question_history\":".len()
        - dudes_proto::MAX_TRACE_BYTES
        - b",\"traces\":".len()
        - dudes_proto::MAX_CHAT_BYTES;
    budget - baseline
}

#[test]
fn browser_look_updates_live_and_archive_and_refuses_stale_edits() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).unwrap();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (before_view, _) = snapshot(addr, &cookie, &csrf);
    let before_agent = &before_view["projection"]["agents"][0];
    assert!(before_agent.get("look_lsn").is_none());
    let body = json!({
        "request_id": request_id(1), "agent": agent,
        "expected_lsn": "0",
        "role": "designer", "character": "h", "accent": "sky",
    });
    let before_records = fixture.records();
    let before_activity = before_view["projection"]["activity"].clone();
    let (head, original) = post_look(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(
        original,
        json!({
            "request_id": request_id(1), "state": "changed", "agent": agent,
            "role": "designer", "character": "h", "accent": "sky",
        })
    );
    let records = fixture.records();
    assert_eq!(records.len(), before_records.len() + 1);
    assert!(matches!(records.last(), Some(Record::AgentLookChanged {
        id, role, character: Some(dudes_types::CastCharacter::H),
        accent: Some(dudes_types::AccentColour::Sky), browser_look: Some(_),
    }) if *id == agent && role == "designer"));
    let (view, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(view["projection"]["agents"][0]["role"], "designer");
    assert_eq!(view["projection"]["agents"][0]["character"], "h");
    assert_eq!(view["projection"]["agents"][0]["accent"], "sky");
    assert_eq!(view["projection"]["activity"], before_activity);
    assert_eq!(post_look(addr, &cookie, &csrf, &body).1, original);
    let mut conflicting = body.clone();
    conflicting["expected_lsn"] = view["projection"]["agents"][0]["look_lsn"].clone();
    let (head, refusal) = post_look(addr, &cookie, &csrf, &conflicting);
    empty_refusal(&head, &refusal, "HTTP/1.1 409 Conflict");
    let mut conflicting = body.clone();
    conflicting["role"] = json!("writer");
    let (head, refusal) = post_look(addr, &cookie, &csrf, &conflicting);
    empty_refusal(&head, &refusal, "HTTP/1.1 409 Conflict");
    let mut stale = body.clone();
    stale["request_id"] = json!(request_id(2));
    stale["role"] = json!("writer");
    let (head, refusal) = post_look(addr, &cookie, &csrf, &stale);
    assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
    assert_eq!(refusal["reason"], "stale_look");
    assert_eq!(fixture.records(), records);
    running
        .handle
        .ask(Ask::Retire(Box::new(RetireParams {
            agent,
            reason: "done".to_owned(),
        })))
        .unwrap();
    let (view, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(view["projection"]["retired_agents"][0]["role"], "designer");
    assert_eq!(view["projection"]["retired_agents"][0]["character"], "h");
    assert_eq!(view["projection"]["retired_agents"][0]["accent"], "sky");
    assert_eq!(post_look(addr, &cookie, &csrf, &body).1, original);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    let daemon = boot(&settings, fake_seams(&settings)).unwrap();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before_retry = fixture.records();
    assert_eq!(post_look(addr, &cookie, &csrf, &body).1, original);
    assert_eq!(fixture.records(), before_retry);
    let (replayed, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(
        replayed["projection"]["retired_agents"][0]["role"],
        "designer"
    );
    assert_eq!(
        replayed["projection"]["retired_agents"][0]["character"],
        "h"
    );
    assert_eq!(replayed["projection"]["retired_agents"][0]["accent"], "sky");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_look_revision_refuses_aba_even_when_display_fields_match_again() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).unwrap();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (initial, _) = snapshot(addr, &cookie, &csrf);
    let original = &initial["projection"]["agents"][0];
    let original_role = original["role"].as_str().unwrap().to_owned();
    let original_lsn = json!("0");
    let first = json!({
        "request_id": request_id(3), "agent": agent, "expected_lsn": original_lsn,
        "role": "designer", "character": "h", "accent": "sky",
    });
    assert!(
        post_look(addr, &cookie, &csrf, &first)
            .0
            .starts_with("HTTP/1.1 200 OK")
    );
    let (changed, _) = snapshot(addr, &cookie, &csrf);
    let changed_lsn = changed["projection"]["agents"][0]["look_lsn"].clone();
    assert_ne!(changed_lsn, original_lsn);
    let back = json!({
        "request_id": request_id(4), "agent": agent, "expected_lsn": changed_lsn,
        "role": original_role, "character": null, "accent": null,
    });
    assert!(
        post_look(addr, &cookie, &csrf, &back)
            .0
            .starts_with("HTTP/1.1 200 OK")
    );
    let (again, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(again["projection"]["agents"][0]["role"], original_role);
    assert_eq!(again["projection"]["agents"][0]["character"], Value::Null);
    assert_eq!(again["projection"]["agents"][0]["accent"], Value::Null);
    let stale = json!({
        "request_id": request_id(5), "agent": agent, "expected_lsn": original_lsn,
        "role": "writer", "character": null, "accent": null,
    });
    let before = fixture.records();
    let (head, refused) = post_look(addr, &cookie, &csrf, &stale);
    assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
    assert_eq!(refused["reason"], "stale_look");
    assert_eq!(fixture.records(), before);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    let daemon = boot(&settings, fake_seams(&settings)).unwrap();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (replayed, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(replayed["projection"]["agents"][0]["role"], original_role);
    assert_eq!(
        replayed["projection"]["agents"][0]["character"],
        Value::Null
    );
    assert_eq!(replayed["projection"]["agents"][0]["accent"], Value::Null);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_look_revocation_and_uncertain_fsync_replay() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).unwrap();
    let pause = daemon.onboarding_pause();
    let failure = daemon.onboarding_failure();
    let authorizer = daemon.floor_session_authorizer().unwrap();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (view, _) = snapshot(addr, &cookie, &csrf);
    assert!(view["projection"]["agents"][0].get("look_lsn").is_none());
    let body = json!({
        "request_id": request_id(6), "agent": agent,
        "expected_lsn": "0",
        "role": "designer", "character": "h", "accent": "sky",
    });
    let before = fixture.records();
    pause.arm(OnboardingPausePoint::BeforeLookPermit);
    let loser = {
        let (cookie, csrf, body) = (cookie.clone(), csrf.clone(), body.clone());
        std::thread::spawn(move || post_look(addr, &cookie, &csrf, &body))
    };
    assert!(pause.wait_reached(Duration::from_secs(5)));
    assert!(authorizer.revoke_for_test(&cookie));
    pause.release();
    let (head, value) = loser.join().unwrap();
    empty_refusal(&head, &value, "HTTP/1.1 403 Forbidden");
    assert_eq!(fixture.records(), before);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    failure.arm(OnboardingFailurePoint::Look);
    let (head, value) = post_look(addr, &cookie, &csrf, &body);
    empty_refusal(&head, &value, "HTTP/1.1 503 Service Unavailable");
    let (head, value) = post_look(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 202 Accepted"), "{head}");
    assert_eq!(value["state"], "pending");
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::AgentLookChanged { .. }))
            .count(),
        1
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    let daemon = boot(&settings, fake_seams(&settings)).unwrap();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    let (head, result) = post_look(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(result["role"], "designer");
    assert_eq!(fixture.records(), before);
    let (recorded_lsn, _) = fixture
        .entries()
        .into_iter()
        .find(
            |(_, entry)| matches!(entry.record, Record::AgentLookChanged { id, .. } if id == agent),
        )
        .unwrap();
    let (replayed, _) = snapshot(addr, &cookie, &csrf);
    let row = &replayed["projection"]["agents"][0];
    assert_eq!(row["role"], "designer");
    assert_eq!(row["character"], "h");
    assert_eq!(row["accent"], "sky");
    assert_eq!(row["look_lsn"], json!(recorded_lsn.0.to_string()));
    let mut stale = body.clone();
    stale["request_id"] = json!(request_id(12));
    stale["role"] = json!("writer");
    let before = fixture.records();
    let (head, refusal) = post_look(addr, &cookie, &csrf, &stale);
    assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
    assert_eq!(refusal["reason"], "stale_look");
    assert_eq!(fixture.records(), before);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_look_switch_and_strict_body_append_nothing() {
    for control in [
        FloorControl::Off,
        FloorControl::On {
            goal: false,
            retire: false,
            renew: false,
        },
    ] {
        let fixture = Fixture::new();
        let mut settings = goal_settings(&fixture, false);
        settings.floor_control = control;
        let daemon = boot(&settings, fake_seams(&settings)).unwrap();
        let addr = daemon.floor_addr().unwrap();
        let mut running = common::start(daemon);
        let agent = common::hire(&running.handle, fixture.state.path());
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let (view, _) = snapshot(addr, &cookie, &csrf);
        assert!(view["projection"]["agents"][0].get("look_lsn").is_none());
        let valid = json!({
            "request_id": request_id(7), "agent": agent,
            "expected_lsn": "0",
            "role": "designer", "character": "h", "accent": "sky",
        });
        let before = fixture.records();
        if control == FloorControl::Off {
            let bytes = serde_json::to_vec(&valid).unwrap();
            assert_eq!(
                raw_post(addr, "/floor/look", &cookie, &csrf, &bytes),
                raw_post(addr, "/floor/unrouted", &cookie, &csrf, &bytes),
            );
        } else {
            let mut invalid = Vec::new();
            for (field, value) in [
                ("character", json!("x")),
                ("accent", json!("red")),
                ("role", json!(" \t\r\n")),
                ("role", json!("é".repeat(257))),
                ("expected_lsn", json!("01")),
            ] {
                let mut body = valid.clone();
                body[field] = value;
                invalid.push(body);
            }
            for field in ["character", "accent"] {
                let mut body = valid.clone();
                body.as_object_mut().unwrap().remove(field);
                invalid.push(body);
            }
            let mut unknown = valid.clone();
            unknown["token"] = json!("not admitted");
            invalid.push(unknown);
            for body in invalid {
                let (head, refusal) = post_look(addr, &cookie, &csrf, &body);
                empty_refusal(&head, &refusal, "HTTP/1.1 400 Bad Request");
            }
            let duplicate = format!(
                "{{\"request_id\":\"{}\",\"agent\":\"{agent}\",\"expected_lsn\":\"1\",\"role\":\"builder\",\"role\":\"writer\",\"character\":null,\"accent\":null}}",
                request_id(8),
            );
            let (head, refusal) = post_bytes(
                addr,
                "/floor/look",
                &cookie,
                &csrf,
                duplicate.as_bytes(),
                None,
            );
            empty_refusal(&head, &refusal, "HTTP/1.1 400 Bad Request");
            let missing = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
            assert_ne!(missing, agent);
            let mut body = valid.clone();
            body["agent"] = json!(missing);
            let (head, refusal) = post_look(addr, &cookie, &csrf, &body);
            assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
            assert_eq!(refusal["reason"], "agent_absent");
        }
        assert_eq!(fixture.records(), before);
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

#[test]
fn a_card_sized_before_chat_was_always_on_replays_and_blocks_only_look_edits() {
    let agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let hired = seeded_look_hire(agent);
    let mut legacy_card = seeded_look_card();
    let Record::CardCreated { title, .. } = &mut legacy_card else {
        unreachable!()
    };
    *title = "x".repeat(787_212);
    let fixture = Fixture::new();
    seed(&fixture, [hired, legacy_card]);
    let settings = goal_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).unwrap();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (frame, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(
        frame["projection"]["cards"][0]["title"]
            .as_str()
            .unwrap()
            .len(),
        787_212,
        "the oversized card still replays and publishes"
    );
    let body = json!({
        "request_id": request_id(9), "agent": agent,
        "expected_lsn": "0", "role": "x".repeat(512),
        "character": "h", "accent": "sky",
    });
    let before = fixture.records();
    let (head, refusal) = post_look(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
    assert_eq!(refusal["reason"], "floor_limit");
    assert_eq!(fixture.records(), before);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_and_local_look_reserve_the_complete_floor_frame() {
    let agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let hired = seeded_look_hire(agent);
    let card = seeded_look_card();
    let at = dudes_types::Timestamp::from_unix_millis(1_787_394_800_000);
    let mut projected = dudes_journal::Projections::in_memory().unwrap();
    for (index, record) in [hired.clone(), card.clone()].into_iter().enumerate() {
        projected
            .apply(
                Lsn(u64::try_from(index + 1).unwrap()),
                0,
                &dudes_proto::Entry { at, record },
            )
            .unwrap();
    }
    let mut candidate = duded::floor::snapshot(&mut projected, at).unwrap();
    candidate.agents[0].role = "x".repeat(512);
    candidate.agents[0].character = Some(dudes_types::CastCharacter::H);
    candidate.agents[0].accent = Some(dudes_types::AccentColour::Sky);
    candidate.agents[0].look_lsn = Some(u64::MAX.to_string());
    let room = look_title_room(candidate);
    assert!(room > 512);

    for over_by in [1_usize, 0] {
        let fixture = Fixture::new();
        let mut near_cap_card = card.clone();
        let Record::CardCreated { title, .. } = &mut near_cap_card else {
            unreachable!()
        };
        *title = "x".repeat(room + over_by);
        seed(&fixture, [hired.clone(), near_cap_card]);
        let settings = goal_settings(&fixture, true);
        let daemon = boot(&settings, fake_seams(&settings)).unwrap();
        let addr = daemon.floor_addr().unwrap();
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let body = json!({
            "request_id": request_id(9 + over_by), "agent": agent,
            "expected_lsn": "0", "role": "x".repeat(512),
            "character": "h", "accent": "sky",
        });
        let before = fixture.records();
        let (head, response) = post_look(addr, &cookie, &csrf, &body);
        if over_by == 1 {
            assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
            assert_eq!(response["reason"], "floor_limit");
            assert_eq!(fixture.records(), before);
            let local = running
                .handle
                .ask(Ask::Look(Box::new(dudes_proto::rpc::agent::LookParams {
                    agent,
                    role: "x".repeat(512),
                    character: Some(dudes_types::CastCharacter::H),
                    accent: Some(dudes_types::AccentColour::Sky),
                })))
                .unwrap_err();
            assert_eq!(local.code, -32004);
            assert_eq!(fixture.records(), before);
        } else {
            assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
            assert_eq!(response["state"], "changed");
            assert_eq!(fixture.records().len(), before.len() + 1);
        }
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
        if over_by == 1 {
            let mut disabled = settings.clone();
            disabled.floor = None;
            let daemon = boot(&disabled, fake_seams(&disabled)).unwrap();
            let mut running = common::start(daemon);
            let before = fixture.records();
            let local = running
                .handle
                .ask(Ask::Look(Box::new(dudes_proto::rpc::agent::LookParams {
                    agent,
                    role: "x".repeat(512),
                    character: Some(dudes_types::CastCharacter::H),
                    accent: Some(dudes_types::AccentColour::Sky),
                })))
                .unwrap_err();
            assert_eq!(local.code, -32004);
            assert_eq!(fixture.records(), before);
            assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
        }
    }
}

#[test]
fn untouched_two_agent_replay_omits_look_revisions_and_first_edit_reserves_them() {
    let agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let first = seeded_look_hire(agent);
    let second = seeded_look_hire(AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap());
    let mut card = seeded_look_card();
    let at = dudes_types::Timestamp::from_unix_millis(1_787_394_800_000);
    let mut projected = dudes_journal::Projections::in_memory().unwrap();
    for (index, record) in [first.clone(), second.clone(), card.clone()]
        .into_iter()
        .enumerate()
    {
        projected
            .apply(
                Lsn(u64::try_from(index + 1).unwrap()),
                0,
                &dudes_proto::Entry { at, record },
            )
            .unwrap();
    }
    let candidate = duded::floor::snapshot(&mut projected, at).unwrap();
    assert!(candidate.agents.iter().all(|row| row.look_lsn.is_none()));
    let room = look_title_room(candidate);
    let Record::CardCreated { title, .. } = &mut card else {
        unreachable!()
    };
    *title = "x".repeat(room - 25);

    let fixture = Fixture::new();
    seed(&fixture, [first, second, card]);
    let settings = goal_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).unwrap();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (view, _) = snapshot(addr, &cookie, &csrf);
    let agents = view["projection"]["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 2);
    assert!(agents.iter().all(|row| row.get("look_lsn").is_none()));
    let before = fixture.records();
    let body = json!({
        "request_id": request_id(11), "agent": agent, "expected_lsn": "0",
        "role": "backend engineer", "character": null, "accent": null,
    });
    let (head, refusal) = post_look(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head}");
    assert_eq!(refusal["reason"], "floor_limit");
    assert_eq!(fixture.records(), before);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_rename_preserves_identity_activity_and_original_retry_after_retirement_and_replay() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).unwrap();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    running.handle.ask(Ask::Pause(agent)).unwrap();
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (before_view, _) = snapshot(addr, &cookie, &csrf);
    let old_name = before_view["projection"]["agents"][0]["name"].clone();
    let old_activity = before_view["projection"]["activity"].clone();
    let before = fixture.records();
    let body = rename_body(&request_id(1), agent, " \tAda renamed\r\n");
    let (head, original) = post_rename(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(
        original,
        json!({"request_id":request_id(1), "state":"changed", "agent":agent, "name":"Ada renamed"})
    );
    let after = fixture.records();
    assert_eq!(
        after.len(),
        before.len() + 1,
        "rename appends only its own record"
    );
    assert!(
        matches!(after.last(), Some(Record::AgentRenamed { id, name, browser_rename: Some(_) }) if *id == agent && name == "Ada renamed")
    );
    let (view, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(view["projection"]["agents"][0]["id"], json!(agent));
    assert_eq!(view["projection"]["agents"][0]["name"], "Ada renamed");
    assert_ne!(old_name, "Ada renamed");
    let activity = view["projection"]["activity"].as_array().unwrap();
    assert_eq!(
        &activity[..activity.len() - 1],
        old_activity.as_array().unwrap()
    );
    assert_eq!(activity.last().unwrap()["kind"], "agent_renamed");
    assert_eq!(activity.last().unwrap()["agent"]["name"], "Ada renamed");
    running
        .handle
        .ask(Ask::Rename(Box::new(
            dudes_proto::rpc::agent::RenameParams {
                agent,
                name: "Later name".to_owned(),
            },
        )))
        .unwrap();
    let before_retry = fixture.records();
    assert_eq!(post_rename(addr, &cookie, &csrf, &body).1, original);
    let (head, conflict) = post_rename(
        addr,
        &cookie,
        &csrf,
        &rename_body(&request_id(1), agent, "Different"),
    );
    empty_refusal(&head, &conflict, "HTTP/1.1 409 Conflict");
    assert_eq!(fixture.records(), before_retry);
    running
        .handle
        .ask(Ask::Retire(Box::new(RetireParams {
            agent,
            reason: "private".to_owned(),
        })))
        .unwrap();
    let retired = fixture.records();
    assert_eq!(post_rename(addr, &cookie, &csrf, &body).1, original);
    let (head, refusal) = post_rename(
        addr,
        &cookie,
        &csrf,
        &rename_body(&request_id(2), agent, "New"),
    );
    assert!(head.starts_with("HTTP/1.1 409 Conflict"));
    assert_eq!(refusal["reason"], "agent_absent");
    assert_eq!(fixture.records(), retired);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    let daemon = boot(&settings, fake_seams(&settings)).unwrap();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    assert_eq!(post_rename(addr, &cookie, &csrf, &body).1, original);
    assert_eq!(fixture.records(), before);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_rename_route_off_missing_agent_and_name_bounds_append_nothing() {
    for control in [
        FloorControl::Off,
        FloorControl::On {
            goal: false,
            retire: false,
            renew: false,
        },
    ] {
        let fixture = Fixture::new();
        let mut settings = goal_settings(&fixture, false);
        settings.floor_control = control;
        let daemon = boot(&settings, fake_seams(&settings)).unwrap();
        let addr = daemon.floor_addr().unwrap();
        let mut running = common::start(daemon);
        let agent = common::hire(&running.handle, fixture.state.path());
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let before = fixture.records();
        let body = serde_json::to_vec(&rename_body(&request_id(1), agent, "New")).unwrap();
        if control == FloorControl::Off {
            assert_eq!(
                raw_post(addr, "/floor/rename", &cookie, &csrf, &body),
                raw_post(addr, "/floor/unrouted", &cookie, &csrf, &body)
            );
        } else {
            for name in [" \t\r\n".to_owned(), "é".repeat(65), "x".repeat(129)] {
                let (head, refused) = post_rename(
                    addr,
                    &cookie,
                    &csrf,
                    &rename_body(&request_id(2), agent, &name),
                );
                empty_refusal(&head, &refused, "HTTP/1.1 400 Bad Request");
            }
            let missing = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
            assert_ne!(missing, agent);
            let (head, refused) = post_rename(
                addr,
                &cookie,
                &csrf,
                &rename_body(&request_id(3), missing, "New"),
            );
            assert!(head.starts_with("HTTP/1.1 409 Conflict"));
            assert_eq!(refused["reason"], "agent_absent");
        }
        assert_eq!(fixture.records(), before);
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

#[test]
fn browser_rename_revocation_before_append_and_uncertain_fsync_replay() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let daemon = boot(&settings, fake_seams(&settings)).unwrap();
    let pause = daemon.onboarding_pause();
    let failure = daemon.onboarding_failure();
    let authorizer = daemon.floor_session_authorizer().unwrap();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = rename_body(&request_id(1), agent, "New");
    let before = fixture.records();
    pause.arm(OnboardingPausePoint::BeforeRenamePermit);
    let loser = {
        let (cookie, csrf, body) = (cookie.clone(), csrf.clone(), body.clone());
        std::thread::spawn(move || post_rename(addr, &cookie, &csrf, &body))
    };
    assert!(pause.wait_reached(Duration::from_secs(5)));
    assert!(authorizer.revoke_for_test(&cookie));
    pause.release();
    let (head, value) = loser.join().unwrap();
    empty_refusal(&head, &value, "HTTP/1.1 403 Forbidden");
    assert_eq!(fixture.records(), before);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    failure.arm(OnboardingFailurePoint::Rename);
    let (head, value) = post_rename(addr, &cookie, &csrf, &body);
    empty_refusal(&head, &value, "HTTP/1.1 503 Service Unavailable");
    let (head, value) = post_rename(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 202 Accepted"));
    assert_eq!(value["state"], "pending");
    let (head, value) = post_rename(
        addr,
        &cookie,
        &csrf,
        &rename_body(&request_id(2), agent, "Fresh"),
    );
    empty_refusal(&head, &value, "HTTP/1.1 503 Service Unavailable");
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|r| matches!(r, Record::AgentRenamed { .. }))
            .count(),
        1
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    let daemon = boot(&settings, fake_seams(&settings)).unwrap();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, value) = post_rename(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(value["name"], "New");
    let (view, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(view["projection"]["agents"][0]["name"], "New");
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|r| matches!(r, Record::AgentRenamed { .. }))
            .count(),
        1
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn rename_is_operator_only_and_refused_local_names_append_nothing() {
    let fixture = Fixture::new();
    let settings = goal_settings(&fixture, true);
    let column = common::ScriptedColumn::new(dudes_proto::PeerProvenance::NoRecordedAgent);
    let parts = seams_with(
        &settings,
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
        Arc::clone(&column) as Arc<dyn dudes_rpc::auth::PeerResolver>,
    );
    let tokens = parts.rng.tokens();
    let daemon = boot(&settings, parts).unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::false_program()]),
    );
    let hash = fixture.await_record("HookTokenMinted", |record| match record {
        Record::HookTokenMinted { token_hash, .. } => Some(token_hash.clone()),
        _ => None,
    });
    let token = tokens.token_of(&hash);
    running.handle.doctor().unwrap();
    column.set(dudes_proto::PeerProvenance::Agent(agent));
    let before = fixture.records();
    let mut client = Client::connect(settings.socket()).unwrap();
    let reply = client
        .call(
            Method::AgentRename,
            Some(json!({
                "agent":agent, "name":"Stolen rename", "token":token.expose(),
            })),
        )
        .unwrap();
    assert_eq!(reply.error.unwrap().code, -32001);
    assert_eq!(fixture.records(), before);
    column.set(dudes_proto::PeerProvenance::NoRecordedAgent);
    let rename = |agent, name: &str| {
        let cred = std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .unwrap();
        Client::connect(settings.socket())
            .unwrap()
            .call_as_operator(
                Method::AgentRename,
                Some(json!({"agent":agent,"name":name})),
                OperatorCredential::new(cred).unwrap(),
            )
            .unwrap()
    };
    for name in [" \t\r\n".to_owned(), "é".repeat(65), "x".repeat(129)] {
        assert_eq!(rename(agent, &name).error.unwrap().code, -32602);
    }
    let absent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    assert_eq!(rename(absent, "New").error.unwrap().code, -32003);
    assert_eq!(fixture.records(), before);
    assert_eq!(rename(agent, " \tRenamed\r\n").result, Some(json!({})));
    let added = fixture.records();
    assert_eq!(added.len(), before.len() + 1);
    assert!(
        matches!(added.last(), Some(Record::AgentRenamed { name, browser_rename: None, .. }) if name == "Renamed")
    );
    running
        .handle
        .ask(Ask::Retire(Box::new(RetireParams {
            agent,
            reason: "private".to_owned(),
        })))
        .unwrap();
    let before = fixture.records();
    assert_eq!(rename(agent, "Retired").error.unwrap().code, -32003);
    assert_eq!(fixture.records(), before);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_resume_clears_an_automatic_hold_and_fences_a_later_episode() {
    let fixture = Fixture::new();
    let settings = control_settings(&fixture, true);
    let mut setup = common::start(boot(&settings, fake_seams(&settings)).expect("setup"));
    let agent = common::hire(&setup.handle, fixture.state.path());
    setup.stop(ExitCause::Ended);
    let hold = || Record::AgentHeld {
        agent,
        provenance: dudes_types::Provenance::Observed,
        cause: dudes_proto::AutomaticHold::LaunchFailed {
            reason: "managed process could not launch (NotFound)".into(),
        },
    };
    seed(&fixture, [hold()]);
    let first_hold = fixture
        .entries()
        .iter()
        .find_map(|(lsn, e)| matches!(e.record, Record::AgentHeld { .. }).then_some(*lsn))
        .unwrap();
    let daemon = boot(&settings, fake_seams(&settings)).expect("held run");
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (view, _) = snapshot(addr, &cookie, &csrf);
    let row = &view["projection"]["agents"][0];
    assert_eq!(row["automatic_hold"]["kind"], "launch_failed");
    assert_eq!(row["control_lsn"], first_hold.0.to_string());
    assert_eq!(row["paused"], false);
    let (head, body) = post_control(
        addr,
        RESUME,
        &cookie,
        &csrf,
        &control_body(&request_id(1), agent, None),
    );
    refused(&head, &body, "stale_control");
    let (head, body) = post_control(
        addr,
        RESUME,
        &cookie,
        &csrf,
        &control_body(&request_id(2), agent, Some(first_hold)),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {body}");
    let (view, _) = snapshot(addr, &cookie, &csrf);
    assert!(
        view["projection"]["agents"][0]
            .get("automatic_hold")
            .is_none()
    );
    running.stop(ExitCause::Ended);
    seed(&fixture, [hold()]);
    let second_hold = fixture
        .entries()
        .iter()
        .rev()
        .find_map(|(lsn, e)| matches!(e.record, Record::AgentHeld { .. }).then_some(*lsn))
        .unwrap();
    assert!(second_hold > first_hold);
    let daemon = boot(&settings, fake_seams(&settings)).expect("later held run");
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, body) = post_control(
        addr,
        RESUME,
        &cookie,
        &csrf,
        &control_body(&request_id(3), agent, Some(first_hold)),
    );
    refused(&head, &body, "stale_control");
    let (view, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(
        view["projection"]["agents"][0]["control_lsn"],
        second_hold.0.to_string()
    );
    assert_eq!(
        view["projection"]["agents"][0]["automatic_hold"]["kind"],
        "launch_failed"
    );
    running.stop(ExitCause::Ended);
}

#[test]
fn browser_settings_view_is_an_exact_read_only_allowlist() {
    let fixture = Fixture::new();
    let mut settings = enrolled_settings(&fixture);
    settings.floor_answer_enabled = true;
    settings.floor_control = FloorControl::On {
        goal: true,
        retire: true,
        renew: true,
    };
    settings.floor_terminal_enabled = true;
    settings.floor_git_enabled = true;
    settings.per_turn_floor = Spend {
        usd: Usd::from_micros(1_234),
        tokens: Tokens::new(9_007_199_254_740_993),
        work_tokens: Tokens::new(9_007_199_254_740_993),
    };
    settings.daemon_env.push(EnvVar::new(
        "PRIVATE_SETTING",
        "planted-settings-env-secret",
    ));
    settings.enrollment.profiles[0].checks[0].label = "private-settings-check-label".into();
    settings.enrollment.profiles[0].checks[0].predicate = Predicate::Command {
        argv: vec![true_program().into(), "planted-settings-argv-secret".into()],
        timeout_s: 10,
    };
    settings.enrollment.profiles[0].agent_budget.tokens_per_day = Some(Tokens::new(123));
    settings.enrollment.profiles[0].goal_budget.tokens = Some(Tokens::new(456));
    let (_, mut running, addr, cookie, csrf) = default_floor(&settings);
    let frame = snapshot(addr, &cookie, &csrf).0;
    assert_eq!(
        frame["settings"],
        json!({
            "switches": { "answer": true, "hire": true, "control": true, "recovery": false, "goal": true, "renew": true, "retire": true, "terminal": true, "git": true },
            "budget": { "floor_usd_per_day_micros": "100000000", "per_turn_usd_micros": "1234", "per_turn_tokens": "9007199254740993" },
            "providers": ["claude"],
            "workspaces": [{ "workspace_id": "repo_1", "label": "Service workspace",
                "agent_usd_per_day_micros": "10000000", "agent_tokens_per_day": "123",
                "goal_usd_micros": "5000000", "goal_tokens": "456" }],
            "about": { "daemon_version": env!("CARGO_PKG_VERSION"), "browser_wire_version": 3 }
        })
    );
    let bytes = frame["settings"].to_string();
    for secret in [
        "planted-settings-env-secret",
        "planted-settings-argv-secret",
        "private-settings-check-label",
    ] {
        assert!(!bytes.contains(secret), "settings disclosed {secret}");
    }
    let before = fixture.records().len();
    let second = snapshot(addr, &cookie, &csrf).0;
    assert_eq!(second["settings"], frame["settings"]);
    assert_eq!(
        fixture.records().len(),
        before,
        "reading settings appends no record"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_settings_survive_unavailable_hire_and_provider_check() {
    let fixture = Fixture::new();
    let mut settings = with_floor(fixture.settings());
    settings.floor_usd_per_day = None;
    settings.floor_hire_enabled = false;
    let (check, mut running, addr, cookie, csrf) = default_floor(&settings);
    check.hold_next();
    let frame = snapshot(addr, &cookie, &csrf).0;
    assert!(frame.get("onboarding").is_none());
    assert!(frame.get("daemon").is_none());
    assert_eq!(frame["settings"]["providers"], json!(["claude"]));
    assert_eq!(frame["settings"]["workspaces"], json!([]));
    assert_eq!(
        frame["settings"]["budget"]["floor_usd_per_day_micros"],
        Value::Null
    );
    assert_eq!(
        frame["settings"]["switches"],
        json!({
            "answer": false, "hire": false, "control": false, "recovery": false, "goal": false, "renew": false,
            "retire": false, "terminal": false, "git": false
        })
    );
    check.release();
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Journal one pending lease at `tree` before the daemon boots, so it reaches the daemon by replay.
fn seed_lease(fixture: &Fixture, tree: &std::path::Path) {
    std::fs::create_dir_all(tree).expect("the leased directory");
    let (mut writer, _) = dudes_journal::open(
        &fixture.journal_dir(),
        dudes_journal::JournalOptions {
            fsync: dudes_journal::Fsync::None,
            ..dudes_journal::JournalOptions::default()
        },
    )
    .expect("journal");
    writer
        .append(&dudes_proto::Entry {
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_600_000),
            record: Record::WorktreeLeased {
                worktree: dudes_types::WorktreeId::from_bytes([9; 16]),
                origin: fixture.state.path().to_path_buf(),
                path: tree.to_path_buf(),
                branch: "dudes/090909090909".into(),
                base: dudes_types::GitCommit::parse("0123456789abcdef0123456789abcdef01234567")
                    .expect("commit"),
                from: None,
            },
        })
        .expect("lease");
}

/// For the second hire producer: the Floor's onboarding hire into the enrolled
/// repository is refused when that repository is above a live lease, before any record is
/// appended. The same hire beside an unrelated lease is the control.
#[test]
fn a_browser_hire_above_a_live_lease_is_refused_before_any_append() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
    seed_lease(
        &fixture,
        &settings.enrollment.profiles[0].cwd.join("leased"),
    );
    let before = fixture.records();
    assert_eq!(before.len(), 1, "a control: only the lease is recorded");
    let (_, mut running, addr, cookie, csrf) = default_floor(&settings);
    let (head, body) = post_onboard(addr, &cookie, &csrf, &hire_request('A', None));
    assert!(head.starts_with("HTTP/1.1 403 Forbidden"), "{head}");
    assert!(body.is_null());
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    assert_eq!(fixture.records(), before, "a refused hire appends nothing");

    let beside = Fixture::new();
    let settings = enrolled_settings(&beside);
    seed_lease(&beside, &beside.state.path().join("another-tree"));
    let (_, mut running, addr, cookie, csrf) = default_floor(&settings);
    let (head, _) = post_onboard(addr, &cookie, &csrf, &hire_request('A', None));
    assert!(head.starts_with("HTTP/1.1 200 OK"), "control: {head}");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The Git tab is a `git` snapshot sibling and a routed `POST /floor/git` exactly when
/// `[floor.git]` is on. Off, the route is any unrouted path, byte for byte, and no sibling is sent.
#[test]
fn the_git_tab_is_a_sibling_and_a_route_only_when_enabled() {
    for enabled in [false, true] {
        let fixture = Fixture::new();
        let mut settings = control_settings(&fixture, false);
        settings.floor_git_enabled = enabled;
        let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let agent = common::hire(&running.handle, fixture.state.path());
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let (view, _) = snapshot(addr, &cookie, &csrf);
        let body = serde_json::to_vec(&json!({ "agent_id": agent })).unwrap();
        let unrouted = raw_post(addr, "/floor/unrouted", &cookie, &csrf, &body);
        let routed = raw_post(addr, "/floor/git", &cookie, &csrf, &body);
        if enabled {
            assert_eq!(view["git"], json!({}), "the sibling is an empty object");
            let text = String::from_utf8_lossy(&routed);
            assert!(text.starts_with("HTTP/1.1 200 OK"), "{text}");
            assert!(
                text.contains("\"state\":\"git_unavailable\""),
                "a daemon whose runner starts no captured child answers a finite state: {text}"
            );
        } else {
            assert!(view.get("git").is_none(), "{view}");
            assert_eq!(
                routed,
                unrouted,
                "disabled is exactly unrouted: {}",
                String::from_utf8_lossy(&routed)
            );
        }
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

// --- Renew a goal that has nothing left ---------------------------------------------------------

const RENEW_ROUTE: &str = "/floor/renew";
const RENEWED_TEXT: &str = "start the second stage";

/// The numbers a page shows for `repo_1` as [`renew_workspace`] enrolls it: three turns and a $1
/// goal cap in micro-dollars, with no token cap.
const SHOWN: Allowance = ("3", Some("1000000"), None);

/// A renewal's hop cap, goal dollars in micro-dollars and goal tokens, as the page restates them.
type Allowance = (&'static str, Option<&'static str>, Option<&'static str>);

/// One `[[floor.onboarding.workspaces]]` entry with this allowance; `None` leaves a unit uncapped.
fn renew_entry(
    id: &str,
    cwd: &std::path::Path,
    hop_cap: u32,
    goal_usd: Option<&str>,
    goal_tokens: Option<u64>,
) -> String {
    let program = serde_json::to_string(true_program()).expect("predicate TOML string");
    let mut entry = format!(
        "\n[[floor.onboarding.workspaces]]\nworkspace_id = '{id}'\nlabel = 'Workspace {id}'\ncompletion_check_label = 'Checks pass'\ncwd = '{}'\ndepartment = 'engineering'\nisolation = 'default'\nhop_cap = {hop_cap}\npredicate_argv = [{program}]\npredicate_timeout_s = 10\n",
        cwd.display()
    );
    if let Some(usd) = goal_usd {
        let _ = writeln!(entry, "goal_usd = {usd}");
    }
    if let Some(tokens) = goal_tokens {
        let _ = writeln!(entry, "goal_tokens = {tokens}");
    }
    entry
}

/// The operator file: the fixture daemon's own budget and Hire, then control, goal change and
/// renewal on, then `entries`.
fn renew_config(entries: &str) -> String {
    format!(
        "{RELOAD_BUDGET}\n[floor.control]\nenabled = true\n\n[floor.goal]\nenabled = true\n\n[floor.renew]\nenabled = true\n{entries}"
    )
}

/// Settings from that operator file, as `main` loads them: its enrollment and its switches.
fn renew_settings(fixture: &Fixture, text: &str) -> duded::Settings {
    let config = fixture.state.path().join("config.toml");
    std::fs::write(&config, text).expect("operator file");
    let operator = load_operator(&config).expect("valid start file");
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings.enrollment = operator.enrollment;
    settings.floor_control = operator.control;
    settings.floor_hire_enabled = operator.hire_enabled;
    settings.operator_config = Some(config);
    settings
}

/// `repo_1` enrolled at its own repository with [`SHOWN`]'s allowance, and that repository.
fn renew_workspace(fixture: &Fixture) -> (duded::Settings, PathBuf) {
    let repo = reload_repo(fixture, "renew-repo");
    let settings = renew_settings(
        fixture,
        &renew_config(&renew_entry("repo_1", &repo, 3, Some("1"), None)),
    );
    (settings, repo)
}

fn renew_body(request_id: &str, agent: AgentId, goal: GoalId, shown: Allowance) -> Value {
    json!({
        "request_id": request_id,
        "agent": agent,
        "goal": goal,
        "text": RENEWED_TEXT,
        "hop_cap": shown.0,
        "goal_usd_micros": shown.1,
        "goal_tokens": shown.2,
    })
}

fn post_renew(addr: SocketAddr, cookie: &str, csrf: &str, body: &Value) -> (String, Value) {
    post_bytes(
        addr,
        RENEW_ROUTE,
        cookie,
        csrf,
        &serde_json::to_vec(body).expect("renew JSON"),
        None,
    )
}

/// A `200` naming the renewed goal; returns it.
fn renewed(head: &str, response: &Value) -> GoalId {
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {response}");
    assert!(head.contains("Cache-Control: no-store"), "{head}");
    assert_eq!(response["state"], "renewed", "{response}");
    GoalId::parse(response["goal"].as_str().expect("a goal id")).expect("a canonical goal id")
}

/// Every browser renewal of `agent`'s goal, with its position, in journal order.
fn renewals(fixture: &Fixture, agent: AgentId) -> Vec<(Lsn, Goal, BrowserGoal)> {
    browser_goals(fixture, agent)
        .into_iter()
        .filter(|(_, _, attribution)| attribution.kind == BrowserGoalKind::Renewal)
        .collect()
}

/// The digest a renewal's request carries, recomputed here rather than taken from the daemon: the
/// change's framing under its own domain, with an uncapped unit framed as an empty value.
fn expected_renew_digest(agent: AgentId, goal: GoalId, text: &str, shown: Allowance) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"dudes.renew.v1\0");
    for (field, value) in [
        ("agent", agent.to_string()),
        ("goal", goal.to_string()),
        ("text", text.to_owned()),
        ("hop_cap", shown.0.to_owned()),
        ("goal_usd_micros", shown.1.unwrap_or_default().to_owned()),
        ("goal_tokens", shown.2.unwrap_or_default().to_owned()),
    ] {
        hasher.update(u16::try_from(field.len()).unwrap().to_be_bytes());
        hasher.update(field.as_bytes());
        hasher.update(u32::try_from(value.len()).unwrap().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.finalize().into()
}

fn dollars(usd: Usd) -> Usage {
    Usage {
        usd: Some(usd),
        ..Usage::ZERO
    }
}

fn work_tokens(count: u64) -> Usage {
    Usage {
        output: Tokens::new(count),
        ..Usage::ZERO
    }
}

const UNCAPPED: GoalBudget = GoalBudget {
    usd: None,
    tokens: None,
};

/// Hire with `hire`, pause, give it a goal of `hop_cap` turns under `budget`, resume it for one
/// turn that reports `usage`, and wait for that turn's verdict and the decision that follows it. A
/// pause holds the agent from then on when `paused`.
fn one_turn(
    native: &NativeTurnFixture<'_>,
    hire: dudes_proto::rpc::agent::HireParams,
    hop_cap: u32,
    budget: GoalBudget,
    usage: Usage,
    paused: bool,
) -> (AgentId, GoalId) {
    let agent = match native
        .running
        .handle
        .ask(Ask::Hire(Box::new(hire)))
        .expect("the hire")
    {
        Answered::Hired(result) => result.agent,
        other => panic!("agent.hire answered {other:?}"),
    };
    let goal = run_one_turn(native, agent, hop_cap, budget, usage, paused);
    (agent, goal)
}

/// [`one_turn`] for an agent already on the floor.
fn run_one_turn(
    native: &NativeTurnFixture<'_>,
    agent: AgentId,
    hop_cap: u32,
    budget: GoalBudget,
    usage: Usage,
    paused: bool,
) -> GoalId {
    let (running, fixture) = (native.running, native.fixture);
    running.handle.ask(Ask::Pause(agent)).expect("local pause");
    let goal = common::set_goal(
        &running.handle,
        GoalSetParams {
            hop_cap,
            budget,
            ..common::goal_params(agent, &[true_program()])
        },
    );
    running
        .handle
        .ask(Ask::Resume(agent))
        .expect("local resume");
    let turn = issued_goal_turn(fixture, goal);
    if paused {
        running.handle.ask(Ask::Pause(agent)).expect("local pause");
    }
    native.complete(turn, Some(usage));
    let ended = await_entry(
        fixture,
        "the turn's end",
        |_, record| matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn),
    );
    // A paused agent decides once on the turn's end and again on its verdict. Waiting for that
    // second decision leaves nothing in flight to land inside the test's next step.
    let judged = await_entry(fixture, "the verdict", |lsn, record| {
        lsn > ended
            && matches!(record, Record::PredicateResult { goal: judged, .. } if *judged == goal)
    });
    await_entry(fixture, "the decision after the verdict", |lsn, record| {
        lsn > judged && matches!(record, Record::LoopDecided { agent: who, .. } if *who == agent)
    });
    goal
}

/// Hire into `cwd` and spend the one turn of a goal with no ceiling at `usd`: nothing is left.
fn spent(
    native: &NativeTurnFixture<'_>,
    cwd: &std::path::Path,
    usd: Usd,
    paused: bool,
) -> (AgentId, GoalId) {
    one_turn(
        native,
        common::hire_params(cwd),
        1,
        UNCAPPED,
        dollars(usd),
        paused,
    )
}

/// The decisions `agent` took after `lsn`, in order.
fn decisions_after(fixture: &Fixture, agent: AgentId, lsn: Lsn) -> Vec<LoopDecision> {
    after(fixture, lsn)
        .into_iter()
        .filter_map(|(_, entry)| match entry.record {
            Record::LoopDecided {
                agent: who,
                decision,
                ..
            } if who == agent => Some(decision),
            _ => None,
        })
        .collect()
}

/// How many Managed turns were issued for `goal`.
fn managed_turns(fixture: &Fixture, goal: GoalId) -> usize {
    fixture
        .records()
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::TurnIssued {
                    goal: issued,
                    mode: dudes_types::Mode::Managed,
                    ..
                } if *issued == goal
            )
        })
        .count()
}

const fn exhausted(reason: dudes_proto::ExhaustedReason) -> LoopDecision {
    LoopDecision::Exhausted { reason }
}

/// The primary test, stub-backed and read from the replayed journal. `repo_1` enrolls the
/// agent's directory with three turns and a $1 goal cap; the agent was hired there locally, and a
/// local goal of one turn with no goal cap spent $2 and went `Exhausted{Hops}`, which a goal
/// change refuses as `goal_exhausted`. The renewal's goal keeps the old completion check and takes
/// the workspace's numbers, not the old goal's; its first decision reads its own spend, so it
/// issues one turn carrying the new text; that $1 turn exhausts the new goal's cap; and after the
/// workspace's hop cap is edited and the daemon restarts, replay keeps the recorded numbers and the
/// index answers the key.
///
/// Mutants: take the hop cap and ceiling from the old goal (step 3 reads a hop cap of 1 and no
/// cap); drop the spend push for the new goal (step 4 reads $2 against $1 and decides
/// `Exhausted{GoalBudget}`); leave the new goal uncapped (step 5 issues a second turn).
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end stub scenario asserts the six ordered steps on one journal"
)]
fn browser_renew_funds_an_exhausted_goal_from_its_workspace() {
    let fixture = Fixture::new();
    let capture = tempfile::tempdir().expect("task capture directory");
    let cost = capture.path().join("cost");
    let (mut settings, repo) = renew_workspace(&fixture);
    settings.daemon_env.push(EnvVar::new(HELPER_ROLE, "1"));
    settings
        .daemon_env
        .push(EnvVar::new(TASK_CAPTURE, capture.path().as_os_str()));
    settings
        .daemon_env
        .push(EnvVar::new(STUB_COST, cost.as_os_str()));
    std::fs::write(&cost, "2.0").expect("the first turn's cost");
    let daemon = boot(
        &settings,
        real_stub_seams_with(&settings, ScriptedRunner::scripted([common::fails()])),
    )
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, &repo);
    let workspace = fixture.await_record("the hire", |record| match record {
        Record::AgentHired { id, workspace, .. } if *id == agent => Some(workspace.clone()),
        _ => None,
    });
    assert_eq!(
        workspace.map(|workspace| workspace.id).as_deref(),
        Some("repo_1"),
        "a local hire in the enrolled directory records its workspace"
    );
    let g1 = common::set_goal(
        &running.handle,
        GoalSetParams {
            hop_cap: 1,
            budget: UNCAPPED,
            ..common::goal_params(agent, &[true_program()])
        },
    );
    fixture.await_record("G1 exhausted", |record| {
        matches!(
            record,
            Record::LoopDecided { agent: who, decision, .. }
                if *who == agent && *decision == exhausted(dudes_proto::ExhaustedReason::Hops)
        )
        .then_some(())
    });
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (view, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(
        view["goal"],
        json!({
            "max_text_bytes": 2048,
            "renew": { "workspaces": [{
                "workspace_id": "repo_1",
                "hop_cap": "3",
                "goal_usd_micros": "1000000",
                "goal_tokens": null,
            }]},
        })
    );
    let (head, response) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_body(&request_id(1), agent, g1, NEW_TEXT),
    );
    refused(&head, &response, "goal_exhausted");

    std::fs::write(&cost, "1.0").expect("the renewed turn's cost");
    let body = renew_body(&request_id(2), agent, g1, SHOWN);
    let (head, response) = post_renew(addr, &cookie, &csrf, &body);
    let g2 = renewed(&head, &response);
    assert_eq!(
        response,
        json!({ "request_id": request_id(2), "state": "renewed", "agent": agent, "goal": g2 })
    );
    let c = renewals(&fixture, agent)
        .first()
        .map(|(lsn, _, _)| *lsn)
        .expect("the renewal is durable");
    await_entry(&fixture, "G2 exhausted", |lsn, record| {
        lsn > c
            && matches!(
                record,
                Record::LoopDecided { agent: who, decision: LoopDecision::Exhausted { .. }, .. }
                    if *who == agent
            )
    });
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let entries = fixture.entries();
    assert_eq!(
        browser_goals(&fixture, agent).len(),
        1,
        "one browser GoalSet"
    );
    let (_, g2_goal, attribution) = renewals(&fixture, agent).pop().expect("checked");
    let g1_goal = entries
        .iter()
        .find_map(|(_, e)| match &e.record {
            Record::GoalSet { goal, .. } if goal.id == g1 => Some(goal.clone()),
            _ => None,
        })
        .expect("G1");
    assert_eq!(g2_goal.id, g2);
    assert_eq!(g2_goal.text, RENEWED_TEXT);
    assert_eq!(g2_goal.predicate, g1_goal.predicate);
    assert_eq!(g2_goal.completion_check, g1_goal.completion_check);
    assert_eq!((g1_goal.hop_cap, g1_goal.budget), (1, UNCAPPED));
    assert_eq!(g2_goal.hop_cap, 3, "the workspace's hop cap, not G1's");
    assert_eq!(
        g2_goal.budget,
        GoalBudget {
            usd: Some(Usd::from_dollars(1)),
            tokens: None,
        },
        "the workspace's goal cap, not G1's"
    );
    let t1 = entries
        .iter()
        .find_map(|(_, e)| match &e.record {
            Record::TurnIssued { goal, turn, .. } if *goal == g1 => Some(*turn),
            _ => None,
        })
        .expect("T1");
    let t1_usage = entries
        .iter()
        .find_map(|(_, e)| match &e.record {
            Record::TurnEnded { turn, usage, .. } if *turn == t1 => *usage,
            _ => None,
        })
        .expect("T1's known usage");
    assert_eq!(t1_usage.usd, Some(Usd::from_dollars(2)));
    assert_eq!(attribution.replaced_goal, g1);
    assert_eq!(
        attribution.reserved,
        Spend {
            usd: Usd::from_dollars(2),
            tokens: t1_usage.total_tokens(),
            work_tokens: t1_usage
                .input
                .saturating_add(t1_usage.output)
                .saturating_add(t1_usage.cache_creation),
        },
        "the budget gate's figure for G1"
    );
    assert_eq!(
        attribution.request_digest.bytes(),
        expected_renew_digest(agent, g1, RENEWED_TEXT, SHOWN)
    );
    let session = <[u8; 32]>::from(<sha2::Sha256 as sha2::Digest>::digest(cookie.as_bytes()));
    assert_eq!(attribution.session_digest.bytes(), session);

    let decisions = decisions_after(&fixture, agent, c);
    assert!(
        matches!(decisions.first(), Some(LoopDecision::Continue { .. })),
        "the renewal's own decision reads G2's spend: {decisions:?}"
    );
    assert_eq!(
        decisions.last(),
        Some(&exhausted(dudes_proto::ExhaustedReason::GoalBudget)),
        "{decisions:?}"
    );
    assert_eq!(managed_turns(&fixture, g2), 1, "one Managed turn names G2");
    let budget_refusals: Vec<_> = after(&fixture, c)
        .into_iter()
        .filter_map(|(_, e)| match e.record {
            Record::BudgetDecision { goal, ceiling, .. } => Some((goal, ceiling.scope)),
            _ => None,
        })
        .collect();
    assert_eq!(
        budget_refusals,
        [(Some(g2), BudgetScope::Goal)],
        "one refusal, against G2's cap"
    );
    let tasks: Vec<String> = std::fs::read_dir(capture.path())
        .expect("capture directory")
        .filter_map(Result::ok)
        .filter(|file| file.path().extension().is_some_and(|ext| ext == "task"))
        .map(|file| std::fs::read_to_string(file.path()).expect("captured task"))
        .collect();
    assert_eq!(tasks.len(), 2, "two stub launches");
    assert_eq!(
        tasks
            .iter()
            .filter(|task| task.contains(RENEWED_TEXT))
            .count(),
        1,
        "exactly the G2 launch carried the new text on its stdin"
    );

    let edited = renew_settings(
        &fixture,
        &renew_config(&renew_entry("repo_1", &repo, 7, Some("1"), None)),
    );
    assert_eq!(edited.enrollment.profiles[0].hop_cap, 7);
    let daemon = boot(&edited, fake_seams(&edited)).expect("replay boot");
    assert_eq!(daemon.world().agents[&agent].goal, Some(g2));
    assert_eq!(
        daemon.world().goals[&g2].goal.hop_cap,
        3,
        "replay keeps the record"
    );
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&edited).bootstrap);
    let before = fixture.records();
    let (head, replayed) = post_renew(addr, &cookie, &csrf, &body);
    assert_eq!(renewed(&head, &replayed), g2);
    assert_eq!(replayed, response);
    assert_eq!(
        fixture.records(),
        before,
        "the index answered; nothing was appended"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// A renewal needs the goal to have nothing left, in each unit. A goal with a turn left, and one
/// with a turn and capped dollars left after a turn spent part of them, each get
/// `goal_not_exhausted` with no `GoalSet`. Controls: a goal with no turns left applies, and so does
/// one whose turns are spent while capped dollars remain, which a change refuses as
/// `goal_exhausted`.
///
/// Mutant: drop the remainder check, and both refused arms append.
#[test]
fn browser_renew_refuses_a_goal_with_anything_left() {
    let fixture = Fixture::new();
    let (settings, repo) = renew_workspace(&fixture);
    let (native_parts, native_host) = native_fixture_seams(&fixture, fake_seams(&settings));
    let daemon = boot(&settings, native_parts).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let turn_left = common::hire(&running.handle, &repo);
    running
        .handle
        .ask(Ask::Pause(turn_left))
        .expect("local pause");
    let turn_left_goal = common::set_goal(
        &running.handle,
        GoalSetParams {
            hop_cap: 1,
            budget: UNCAPPED,
            ..common::goal_params(turn_left, &[true_program()])
        },
    );
    let five = GoalBudget {
        usd: Some(Usd::from_dollars(5)),
        tokens: None,
    };
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (both_left, both_left_goal) = one_turn(
        &native,
        common::hire_params(&repo),
        2,
        five,
        dollars(Usd::from_dollars(1)),
        true,
    );
    let (spent_turns, spent_turns_goal) = one_turn(
        &native,
        common::hire_params(&repo),
        1,
        five,
        dollars(Usd::from_dollars(1)),
        true,
    );
    let (control, control_goal) = spent(&native, &repo, Usd::from_cents(1), true);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    for (k, agent, goal) in [
        (1, turn_left, turn_left_goal),
        (2, both_left, both_left_goal),
    ] {
        let (head, response) = post_renew(
            addr,
            &cookie,
            &csrf,
            &renew_body(&request_id(k), agent, goal, SHOWN),
        );
        refused(&head, &response, "goal_not_exhausted");
    }
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let (head, response) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_body(&request_id(3), spent_turns, spent_turns_goal, NEW_TEXT),
    );
    refused(&head, &response, "goal_exhausted");
    for (k, agent, goal) in [
        (4, control, control_goal),
        (5, spent_turns, spent_turns_goal),
    ] {
        let (head, response) = post_renew(
            addr,
            &cookie,
            &csrf,
            &renew_body(&request_id(k), agent, goal, SHOWN),
        );
        renewed(&head, &response);
        assert_eq!(renewals(&fixture, agent).len(), 1, "control: {agent}");
    }
    for agent in [turn_left, both_left] {
        assert!(browser_goals(&fixture, agent).is_empty(), "{agent}");
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// With every ceiling unset, the hop cap is the only bound: a one-turn goal renews from a
/// workspace of three turns, and exactly three Managed turns name the new goal before
/// `Exhausted{Hops}`.
#[test]
fn browser_renew_with_no_ceilings_is_bounded_by_the_hop_cap() {
    let fixture = Fixture::new();
    let repo = reload_repo(&fixture, "renew-repo");
    let mut settings = renew_settings(
        &fixture,
        &renew_config(&renew_entry("repo_1", &repo, 3, None, None)),
    );
    settings.floor_usd_per_day = None;
    let (native_parts, native_host) = native_fixture_seams(&fixture, fake_seams(&settings));
    let daemon = boot(&settings, native_parts).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let uncapped_hire = dudes_proto::rpc::agent::HireParams {
        budget: AgentBudget {
            usd_per_day: None,
            tokens_per_day: None,
        },
        ..common::hire_params(&repo)
    };
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (agent, g1) = one_turn(
        &native,
        uncapped_hire,
        1,
        UNCAPPED,
        dollars(Usd::from_dollars(1)),
        false,
    );
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, response) = post_renew(
        addr,
        &cookie,
        &csrf,
        &renew_body(&request_id(1), agent, g1, ("3", None, None)),
    );
    let g2 = renewed(&head, &response);
    for n in 0..3 {
        let turn = nth_turn(&fixture, g2, n);
        native.complete(turn, Some(dollars(Usd::from_dollars(1))));
        let ended = await_entry(
            &fixture,
            "the turn's end",
            |_, record| matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn),
        );
        await_entry(&fixture, "the decision after that turn", |lsn, record| {
            lsn > ended && matches!(record, Record::LoopDecided { agent: who, .. } if *who == agent)
        });
    }
    let c = renewals(&fixture, agent)[0].0;
    let decisions = decisions_after(&fixture, agent, c);
    assert_eq!(
        decisions.last(),
        Some(&exhausted(dudes_proto::ExhaustedReason::Hops)),
        "{decisions:?}"
    );
    assert_eq!(managed_turns(&fixture, g2), 3, "the hop cap bounds G2");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The primary test's shape in tokens: a one-turn goal with no cap spent 2,000 work tokens;
/// the workspace caps a goal at 1,000. The renewal's first decision reads its own spend and issues
/// one turn; that turn's 1,000 tokens exhaust the new goal's cap with no second turn.
#[test]
fn browser_renew_goal_token_ceiling_binds_the_renewed_goal() {
    let fixture = Fixture::new();
    let repo = reload_repo(&fixture, "renew-repo");
    let settings = renew_settings(
        &fixture,
        &renew_config(&renew_entry("repo_1", &repo, 3, None, Some(1_000))),
    );
    let (native_parts, native_host) = native_fixture_seams(&fixture, fake_seams(&settings));
    let daemon = boot(&settings, native_parts).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (agent, g1) = one_turn(
        &native,
        common::hire_params(&repo),
        1,
        UNCAPPED,
        work_tokens(2_000),
        false,
    );
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, response) = post_renew(
        addr,
        &cookie,
        &csrf,
        &renew_body(&request_id(1), agent, g1, ("3", None, Some("1000"))),
    );
    let g2 = renewed(&head, &response);
    let (_, goal, _) = renewals(&fixture, agent).pop().expect("the renewal");
    assert_eq!(
        goal.budget,
        GoalBudget {
            usd: None,
            tokens: Some(Tokens::new(1_000)),
        }
    );
    let turn = fixture.await_record("G2's turn", |record| match record {
        Record::TurnIssued { goal, turn, .. } if *goal == g2 => Some(*turn),
        _ => None,
    });
    native.complete(turn, Some(work_tokens(1_000)));
    let c = renewals(&fixture, agent)[0].0;
    await_entry(&fixture, "G2 exhausted", |lsn, record| {
        lsn > c
            && matches!(
                record,
                Record::LoopDecided { agent: who, decision: LoopDecision::Exhausted { .. }, .. }
                    if *who == agent
            )
    });
    let decisions = decisions_after(&fixture, agent, c);
    assert!(
        matches!(decisions.first(), Some(LoopDecision::Continue { .. })),
        "{decisions:?}"
    );
    assert_eq!(
        decisions.last(),
        Some(&exhausted(dudes_proto::ExhaustedReason::GoalBudget)),
        "{decisions:?}"
    );
    assert_eq!(managed_turns(&fixture, g2), 1);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The `n`th turn issued for `goal`, counting from zero, once it is on the disk.
fn nth_turn(fixture: &Fixture, goal: GoalId, n: usize) -> TurnId {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let turn = fixture
            .records()
            .into_iter()
            .filter_map(|record| match record {
                Record::TurnIssued {
                    goal: issued, turn, ..
                } if issued == goal => Some(turn),
                _ => None,
            })
            .nth(n);
        if let Some(turn) = turn {
            return turn;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no turn {n} for {goal} within 10s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A daily ceiling that the old goal reached still binds its renewal: the renewal applies, its
/// decision is `Exhausted` with `reason` beside a `BudgetDecision` and no turn, and after a beat on
/// the next UTC day a turn for the new goal follows. `hire` and `spend` reach the ceiling.
///
/// Mutant: skip the budget gate on the decision after a renewal, and a turn issues on the first day.
fn renewal_waits_for_the_next_day(
    fixture: &Fixture,
    settings: &duded::Settings,
    hire: dudes_proto::rpc::agent::HireParams,
    spend: Usage,
    reason: dudes_proto::ExhaustedReason,
) {
    let clock = Arc::new(TestClock::new());
    let (native_parts, native_host) = native_fixture_seams(
        fixture,
        seams(
            settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    );
    let daemon = boot(settings, native_parts).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let native = NativeTurnFixture {
        running: &running,
        fixture,
        host: &native_host,
    };
    let (agent, g1) = one_turn(&native, hire, 1, UNCAPPED, spend, false);
    assert_eq!(
        decisions_after(fixture, agent, Lsn::ZERO).last(),
        Some(&exhausted(reason)),
        "the old goal stopped at the daily ceiling"
    );
    let (cookie, csrf) = exchange(addr, &issue(settings).bootstrap);
    let (head, response) = post_renew(
        addr,
        &cookie,
        &csrf,
        &renew_body(&request_id(1), agent, g1, SHOWN),
    );
    let g2 = renewed(&head, &response);
    let c = renewals(fixture, agent)[0].0;
    await_entry(fixture, "the renewal's decision", |lsn, record| {
        lsn > c && matches!(record, Record::LoopDecided { agent: who, .. } if *who == agent)
    });
    assert_eq!(
        decisions_after(fixture, agent, c),
        [exhausted(reason)],
        "the renewal's own decision is held at the ceiling"
    );
    assert_eq!(
        after(fixture, c)
            .iter()
            .filter(|(_, e)| matches!(&e.record, Record::BudgetDecision { agent: who, .. } if *who == agent))
            .count(),
        1,
        "one BudgetDecision"
    );
    assert_eq!(managed_turns(fixture, g2), 0, "no turn on the first day");

    clock.advance(86_400_000);
    running.handle.input(Input::Tick);
    nth_turn(fixture, g2, 0);
    assert_eq!(managed_turns(fixture, g2), 1, "one turn on the next day");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The agent's daily ceiling, in dollars and then in tokens.
#[test]
fn browser_renew_waits_for_the_agent_ceiling() {
    for tokens in [false, true] {
        let fixture = Fixture::new();
        let (settings, repo) = renew_workspace(&fixture);
        let (budget, spend) = if tokens {
            (
                AgentBudget {
                    usd_per_day: None,
                    tokens_per_day: Some(Tokens::new(1_000)),
                },
                work_tokens(1_000),
            )
        } else {
            (
                AgentBudget {
                    usd_per_day: Some(Usd::from_dollars(2)),
                    tokens_per_day: None,
                },
                dollars(Usd::from_dollars(2)),
            )
        };
        let hire = dudes_proto::rpc::agent::HireParams {
            budget,
            ..common::hire_params(&repo)
        };
        renewal_waits_for_the_next_day(
            &fixture,
            &settings,
            hire,
            spend,
            dudes_proto::ExhaustedReason::AgentBudget,
        );
    }
}

/// The floor's daily dollar ceiling.
#[test]
fn browser_renew_waits_for_the_floor_ceiling() {
    let fixture = Fixture::new();
    let (mut settings, repo) = renew_workspace(&fixture);
    settings.floor_usd_per_day = Some(Usd::from_dollars(2));
    renewal_waits_for_the_next_day(
        &fixture,
        &settings,
        common::hire_params(&repo),
        dollars(Usd::from_dollars(2)),
        dudes_proto::ExhaustedReason::FloorBudget,
    );
}

/// A runnable, empty stand-in for `name` first on the managed child's `PATH`, so the daemon
/// resolves that engine's program; the fake hosts these tests use never run it.
fn stand_in_engine(fixture: &Fixture, name: &str) {
    let program = fixture.runtime.path().join("bin").join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    });
    std::fs::write(&program, b"").expect("a stand-in engine");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("a runnable stand-in");
    }
}

/// The billing-credential guard reads the workspace's numbers for the agent. A Codex agent hired
/// in `repo_1` while it capped goal tokens spent its goal on a run with no billing key. With
/// `OPENAI_API_KEY` visible and `repo_1` reloaded to cap only dollars, which do not bound Codex,
/// a renewal is `workspace_unavailable` with no `GoalSet`. Control: reloaded back to a goal token
/// cap, a renewal presenting the new numbers applies, and its decision is not
/// `Exhausted{CeilingRequired}`.
///
/// Mutant: drop the guard from the workspace check, and a `GoalSet` lands followed by
/// `Exhausted{CeilingRequired}`.
#[test]
fn browser_renew_refuses_under_the_billing_guard() {
    let fixture = Fixture::new();
    let repo = reload_repo(&fixture, "renew-repo");
    let tokens = renew_config(&renew_entry("repo_1", &repo, 3, None, Some(1_000)));
    let dollars_only = renew_config(&renew_entry("repo_1", &repo, 3, Some("1"), None));
    let mut settings = renew_settings(&fixture, &tokens);
    stand_in_engine(&fixture, "codex");
    let codex_seams = |settings: &duded::Settings| Seams {
        adapters: Adapters::new([
            Box::new(ClaudeAdapter::default()) as Box<dyn Adapter>,
            Box::new(CodexAdapter::default()),
        ]),
        ..fake_seams(settings)
    };
    let (native_parts, native_host) = native_fixture_seams(&fixture, codex_seams(&settings));
    let mut running = common::start(boot(&settings, native_parts).expect("boot without a key"));
    let hire = dudes_proto::rpc::agent::HireParams {
        provider: Provider::Codex,
        budget: AgentBudget {
            usd_per_day: None,
            tokens_per_day: None,
        },
        ..common::hire_params(&repo)
    };
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (agent, g1) = one_turn(
        &native,
        hire,
        1,
        GoalBudget {
            usd: None,
            tokens: Some(Tokens::new(1_000)),
        },
        work_tokens(10),
        false,
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    settings.daemon_env.push(EnvVar::new(
        "OPENAI_API_KEY",
        "fixture-value-not-a-credential",
    ));
    let daemon = boot(&settings, codex_seams(&settings)).expect("boot with a key");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let sha = rewrite(&settings, &dollars_only);
    operator_reload(&settings, apply(&sha, &[], &[], &["repo_1"])).expect("reload applies");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    let (head, response) = post_renew(
        addr,
        &cookie,
        &csrf,
        &renew_body(&request_id(1), agent, g1, SHOWN),
    );
    refused(&head, &response, "workspace_unavailable");
    assert_eq!(fixture.records(), before, "no GoalSet");

    let sha = rewrite(&settings, &tokens);
    operator_reload(&settings, apply(&sha, &[], &[], &["repo_1"])).expect("reload applies");
    let (head, response) = post_renew(
        addr,
        &cookie,
        &csrf,
        &renew_body(&request_id(2), agent, g1, ("3", None, Some("1000"))),
    );
    renewed(&head, &response);
    let c = renewals(&fixture, agent)[0].0;
    await_entry(&fixture, "the renewal's decision", |lsn, record| {
        lsn > c && matches!(record, Record::LoopDecided { agent: who, .. } if *who == agent)
    });
    assert_ne!(
        decisions_after(&fixture, agent, c).first(),
        Some(&exhausted(dudes_proto::ExhaustedReason::CeilingRequired)),
        "control: a goal token cap bounds Codex"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// A renewal funds only an agent whose recorded workspace still enrolls the directory it was hired
/// from. An agent hired in an unenrolled directory, an agent after its workspace was removed by a
/// reload, and one after a reload re-pointed the workspace's id at another directory each get
/// `workspace_unavailable`; one presenting the numbers a reload replaced gets `profile_changed`.
/// None appends a `GoalSet`. Control: an agent hired into a worktree whose lease came from the
/// enrolled directory applies, although its own directory is the tree.
///
/// Mutants: compare the agent's directory instead of its lease's origin, and the control is
/// refused; drop the directory check, and the re-pointed arm appends.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one daemon walks the workspace through each reload in turn"
)]
fn browser_renew_refuses_unenrolled_moved_and_changed_workspaces() {
    let fixture = Fixture::new();
    let (settings, repo) = renew_workspace(&fixture);
    let unenrolled_dir = reload_repo(&fixture, "unenrolled");
    let moved = reload_repo(&fixture, "moved");
    let tree = fixture.state.path().join("trees").join("leased");
    std::fs::create_dir_all(&tree).expect("the leased tree");
    // Below every id this run mints, so a later hire is still the newest agent.
    let leased = AgentId::from_parts(0, 32);
    let lease = dudes_types::WorktreeId::from_bytes([9; 16]);
    seed(
        &fixture,
        [
            Record::WorktreeLeased {
                worktree: lease,
                origin: repo.clone(),
                path: tree.clone(),
                branch: "dudes/090909090909".into(),
                base: dudes_types::GitCommit::parse("0123456789abcdef0123456789abcdef01234567")
                    .expect("commit"),
                from: None,
            },
            Record::AgentHired {
                character: None,
                accent: None,
                id: leased,
                name: "Kim".to_owned(),
                role: "engineer".to_owned(),
                department: "engineering".to_owned(),
                provider: Provider::Claude,
                cwd: tree,
                budget: AgentBudget {
                    usd_per_day: Some(Usd::from_dollars(10)),
                    tokens_per_day: None,
                },
                sandbox: None,
                isolation: None,
                max_turns: None,
                max_budget: None,
                trusted: false,
                workspace: Some(dudes_proto::record::RecordedWorkspace {
                    id: "repo_1".to_owned(),
                    label: "Workspace repo_1".to_owned(),
                }),
                worktree: Some(lease),
                mcp_servers: Vec::new(),
                browser_onboarding: None,
            },
        ],
    );
    let (native_parts, native_host) = native_fixture_seams(&fixture, fake_seams(&settings));
    let daemon = boot(&settings, native_parts).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let leased_goal = run_one_turn(
        &native,
        leased,
        1,
        UNCAPPED,
        dollars(Usd::from_cents(1)),
        true,
    );
    let (unenrolled, unenrolled_goal) = spent(&native, &unenrolled_dir, Usd::from_cents(1), true);
    let (agent, goal) = spent(&native, &repo, Usd::from_cents(1), true);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let mut k = 0;
    let mut renew = |agent, goal, shown| {
        k += 1;
        post_renew(
            addr,
            &cookie,
            &csrf,
            &renew_body(&request_id(k), agent, goal, shown),
        )
    };

    let before = fixture.records();
    let (head, response) = renew(unenrolled, unenrolled_goal, SHOWN);
    refused(&head, &response, "workspace_unavailable");
    assert_eq!(fixture.records(), before, "an unenrolled directory");

    let (head, response) = renew(leased, leased_goal, SHOWN);
    renewed(&head, &response);
    assert_eq!(
        renewals(&fixture, leased).len(),
        1,
        "control: the lease's origin is the enrolled directory"
    );

    let changed = renew_config(&renew_entry("repo_1", &repo, 7, Some("1"), None));
    let sha = rewrite(&settings, &changed);
    operator_reload(&settings, apply(&sha, &[], &[], &["repo_1"])).expect("reload applies");
    let before = fixture.records();
    let (head, response) = renew(agent, goal, SHOWN);
    refused(&head, &response, "profile_changed");
    assert_eq!(fixture.records(), before, "the page's old numbers");

    let repointed = renew_config(&renew_entry("repo_1", &moved, 7, Some("1"), None));
    let sha = rewrite(&settings, &repointed);
    operator_reload(&settings, apply(&sha, &[], &[], &["repo_1"])).expect("reload applies");
    let (head, response) = renew(agent, goal, ("7", Some("1000000"), None));
    refused(&head, &response, "workspace_unavailable");
    assert_eq!(fixture.records(), before, "an id re-pointed elsewhere");

    let sha = rewrite(&settings, &renew_config(""));
    operator_reload(&settings, apply(&sha, &[], &["repo_1"], &[])).expect("reload applies");
    let (head, response) = renew(agent, goal, ("7", Some("1000000"), None));
    refused(&head, &response, "workspace_unavailable");
    assert_eq!(fixture.records(), before, "a removed workspace");
    for agent in [unenrolled, agent] {
        assert!(browser_goals(&fixture, agent).is_empty(), "{agent}");
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// A goal that went `Done` on its last allowed turn has nothing left and renews like any other:
/// the new goal's decision is `Continue`, not `Done`, because the old goal's verdict cannot end it.
#[test]
fn browser_renew_restarts_a_done_goal_at_its_cap() {
    let fixture = Fixture::new();
    let (settings, repo) = renew_workspace(&fixture);
    let (native_parts, native_host) = native_fixture_seams(
        &fixture,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::scripted([
                common::passes(),
                common::fails(),
            ])),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    );
    let daemon = boot(&settings, native_parts).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (agent, g1) = spent(&native, &repo, Usd::from_cents(1), false);
    assert_eq!(
        decisions_after(&fixture, agent, Lsn::ZERO).last(),
        Some(&LoopDecision::Done),
        "G1 passed its check on its one turn"
    );
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, response) = post_renew(
        addr,
        &cookie,
        &csrf,
        &renew_body(&request_id(1), agent, g1, SHOWN),
    );
    let g2 = renewed(&head, &response);
    nth_turn(&fixture, g2, 0);
    let c = renewals(&fixture, agent)[0].0;
    let decisions = decisions_after(&fixture, agent, c);
    assert!(
        matches!(decisions.first(), Some(LoopDecision::Continue { .. })),
        "{decisions:?}"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "three restart cuts share one scenario and one assertion shape"
)]
fn browser_renew_restart_cut_points_replay_one_record() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Cut {
        GoalSetFsync,
        GoalSetFold,
        TurnIssuedFsync,
    }
    for cut in [Cut::GoalSetFsync, Cut::GoalSetFold, Cut::TurnIssuedFsync] {
        let fixture = Fixture::new();
        let (settings, repo) = renew_workspace(&fixture);
        let run = || {
            let (parts, host) = native_fixture_seams(
                &fixture,
                seams(
                    &settings,
                    SharedHost::new(),
                    Arc::new(ScriptedRunner::default()),
                    Arc::new(TestClock::new()),
                    Witness::default(),
                ),
            );
            (boot(&settings, parts).expect("boot Floor"), host)
        };
        let (daemon, host) = run();
        let failure = daemon.onboarding_failure();
        let append_failure = daemon.append_failure();
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let native = NativeTurnFixture {
            running: &running,
            fixture: &fixture,
            host: &host,
        };
        // Unpaused, the renewal's own decision issues G2's turn; paused, it is a Hold.
        let (agent, goal) = spent(
            &native,
            &repo,
            Usd::from_cents(1),
            cut != Cut::TurnIssuedFsync,
        );
        let (other, other_goal) = spent(&native, &repo, Usd::from_cents(1), true);
        let spawned_before = host.spawned().len();
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let body = renew_body(&request_id(1), agent, goal, SHOWN);
        match cut {
            Cut::GoalSetFsync => failure.arm(OnboardingFailurePoint::GoalChange),
            Cut::GoalSetFold => append_failure.fail_nth(NonZeroUsize::new(2).unwrap()),
            Cut::TurnIssuedFsync => failure.arm(OnboardingFailurePoint::TurnIssued),
        }
        let (head, response) = post_renew(addr, &cookie, &csrf, &body);
        if cut == Cut::GoalSetFsync {
            empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
            let (head, pending) = post_renew(addr, &cookie, &csrf, &body);
            assert!(head.starts_with("HTTP/1.1 202 Accepted"), "{head}");
            assert_eq!(
                pending["state"], "pending",
                "an uncertain append keeps its key"
            );
            let fresh = renew_body(&request_id(2), other, other_goal, SHOWN);
            let (head, response) = post_renew(addr, &cookie, &csrf, &fresh);
            empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
        } else {
            renewed(&head, &response);
        }
        if cut == Cut::TurnIssuedFsync {
            let goal = renewals(&fixture, agent).last().unwrap().1.id;
            nth_turn(&fixture, goal, 0);
            running.handle.doctor().unwrap();
        }
        assert_eq!(
            host.spawned().len(),
            spawned_before,
            "{cut:?}: the cut is before any Spawn"
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
        let (c, g2_goal, _) = renewals(&fixture, agent)
            .pop()
            .expect("the GoalSet is durable");
        match cut {
            Cut::GoalSetFsync | Cut::GoalSetFold => assert!(
                decisions_after(&fixture, agent, c).is_empty(),
                "{cut:?}: no decision after the renewal yet"
            ),
            Cut::TurnIssuedFsync => assert_eq!(
                managed_turns(&fixture, g2_goal.id),
                1,
                "{cut:?}: G2's turn is durable"
            ),
        }

        let (daemon, restarted_host) = run();
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        if cut == Cut::TurnIssuedFsync {
            recover_owed_browser_turn(&settings, &fixture, &running, agent, g2_goal.id);
            issued_goal_turn(&fixture, g2_goal.id);
            assert_eq!(
                restarted_host.spawned().len(),
                1,
                "G2's exact owed turn re-issued once"
            );
        }
        let (head, response) = post_renew(addr, &cookie, &csrf, &body);
        assert_eq!(renewed(&head, &response), g2_goal.id, "{cut:?}");
        assert_eq!(renewals(&fixture, agent).len(), 1, "{cut:?}");
        assert!(browser_goals(&fixture, other).is_empty(), "{cut:?}");
        assert!(
            !decisions_after(&fixture, agent, c).is_empty(),
            "{cut:?}: replay decided on G2"
        );
        if cut == Cut::TurnIssuedFsync {
            assert_eq!(
                managed_turns(&fixture, g2_goal.id),
                1,
                "{cut:?}: G2's turn re-issued exactly once"
            );
            assert_eq!(
                host.spawned().len() + restarted_host.spawned().len(),
                spawned_before + 1,
                "{cut:?}: one Spawn"
            );
        } else {
            assert_eq!(
                managed_turns(&fixture, g2_goal.id),
                0,
                "{cut:?}: the paused agent issues nothing"
            );
            assert_eq!(host.spawned().len(), spawned_before, "{cut:?}");
        }
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

/// Every browser credential is checked before any effect: a wrong Host, a wrong or missing Origin,
/// an unknown cookie, a wrong or missing CSRF and an expired session each get the uniform empty
/// `403`, and the journal does not move. Control: the right credentials apply.
#[test]
fn browser_renew_refuses_wrong_session_host_origin_and_csrf_without_effect() {
    let fixture = Fixture::new();
    let (settings, repo) = renew_workspace(&fixture);
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let (native_parts, native_host) = native_fixture_seams(
        &fixture,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::clone(&clock),
            Witness::default(),
        ),
    );
    let daemon = boot(&settings, native_parts).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (agent, goal) = spent(&native, &repo, Usd::from_cents(1), true);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = serde_json::to_vec(&renew_body(&request_id(1), agent, goal, SHOWN)).unwrap();
    let host = addr.to_string();
    let request = |host: &str, origin: Option<&str>, cookie: &str, csrf: Option<&str>| {
        let mut head = format!("POST {RENEW_ROUTE} HTTP/1.1\r\nHost: {host}\r\n");
        if let Some(origin) = origin {
            let _ = write!(head, "Origin: {origin}\r\n");
        }
        let _ = write!(head, "Cookie: dudes_floor_session={cookie}\r\n");
        if let Some(csrf) = csrf {
            let _ = write!(head, "X-Dudes-CSRF: {csrf}\r\n");
        }
        let _ = write!(
            head,
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&body);
        bytes
    };
    let origin = format!("http://{host}");
    let unknown = "A".repeat(43);
    let before = fixture.records();
    for (label, bytes) in [
        (
            "wrong Host",
            request("127.0.0.1:1", Some(&origin), &cookie, Some(&csrf)),
        ),
        (
            "wrong Origin",
            request(&host, Some("http://127.0.0.1:1"), &cookie, Some(&csrf)),
        ),
        ("missing Origin", request(&host, None, &cookie, Some(&csrf))),
        (
            "unknown cookie",
            request(&host, Some(&origin), &unknown, Some(&csrf)),
        ),
        (
            "wrong CSRF",
            request(&host, Some(&origin), &cookie, Some(&unknown)),
        ),
        ("missing CSRF", request(&host, Some(&origin), &cookie, None)),
    ] {
        let (head, response) = raw_onboard(addr, &bytes);
        assert!(
            head.starts_with("HTTP/1.1 403 Forbidden"),
            "{label}: {head}"
        );
        assert!(response.is_empty(), "{label}: no envelope");
    }
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let (expired_cookie, expired_csrf) = exchange(addr, &issue(&settings).bootstrap);
    clock.advance(30 * 60 * 1000);
    let (fresh_cookie, fresh_csrf) = exchange(addr, &issue(&settings).bootstrap);
    let renewal = renew_body(&request_id(1), agent, goal, SHOWN);
    let (head, response) = post_renew(addr, &expired_cookie, &expired_csrf, &renewal);
    empty_refusal(&head, &response, "HTTP/1.1 403 Forbidden");
    assert_eq!(
        fixture.records(),
        before,
        "an expired session appends nothing"
    );
    let _ = (cookie, csrf);

    let (head, response) = post_renew(addr, &fresh_cookie, &fresh_csrf, &renewal);
    renewed(&head, &response);
    assert_eq!(renewals(&fixture, agent).len(), 1);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The reasons shared with a goal change, before the remainder, in order. An unknown and a retired
/// agent are `agent_absent`; a halted one is `agent_halted`; an older `GoalId` after a local
/// `goal.set`, and any `GoalId` for an agent with no goal, are `stale_goal`; a turn issued and not
/// yet spawned, and a spawned turn, are each `turn_open`. None appends a `GoalSet`. Control: a
/// paused agent whose goal has nothing left applies, and its decision is `Hold{Paused}`.
#[test]
fn browser_renew_refuses_absent_halted_stale_and_open_turn() {
    let program = true_program();
    let fixture = Fixture::new();
    let (settings, repo) = renew_workspace(&fixture);
    let host = common::UnconfirmedHost::with_native_evidence(true, fixture.journal_dir());
    let native_host = host.shared();
    let native_parts = seams(
        &settings,
        host.clone(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    native_host.bind_child_ends(Arc::clone(&native_parts.children));
    let daemon = boot(&settings, native_parts).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);

    // The first launch is left unconfirmed, so its turn stays issued and never spawned.
    let issued = common::hire(&running.handle, &repo);
    let issued_goal = common::set_goal(&running.handle, common::goal_params(issued, &[program]));
    let issued_turn = nth_turn(&fixture, issued_goal, 0);
    fixture.await_record("the first turn's actual unconfirmed cleanup", |record| {
        matches!(record, Record::InstructionObservationFinished {
            owner: dudes_proto::InstructionObservationOwner::Managed { agent, turn, .. },
            settlement: dudes_proto::InstructionSettlement::Unknown {
                reason: dudes_proto::InstructionUnknownReason::CleanupUnconfirmed,
                end: None, sampled_at: None, recovery: false, ..
            }, ..
        } if *agent == issued && *turn == issued_turn)
        .then_some(())
    });
    let spawned = common::hire(&running.handle, &repo);
    let spawned_goal = common::set_goal(&running.handle, common::goal_params(spawned, &[program]));
    fixture.await_record("the spawned turn", |record| {
        matches!(record, Record::TurnSpawned { agent: who, .. } if *who == spawned).then_some(())
    });
    assert_eq!(host.children(), 1, "one child exists: the spawned turn's");

    let (retired, retired_goal) = paused_with_goal(&running, &fixture);
    running
        .handle
        .ask(Ask::Retire(Box::new(RetireParams {
            agent: retired,
            reason: "done".to_owned(),
        })))
        .expect("retire");
    let (halted, halted_goal) = paused_with_goal(&running, &fixture);
    running.handle.ask(Ask::Halt(halted)).expect("local halt");
    let (stale, older) = paused_with_goal(&running, &fixture);
    let newer = common::set_goal(&running.handle, common::goal_params(stale, &[program]));
    let goalless = common::hire(&running.handle, fixture.state.path());
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (control, control_goal) = spent(&native, &repo, Usd::from_cents(1), true);
    let unknown = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("an agent id");

    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    for (k, agent, goal, reason) in [
        (1, unknown, older, "agent_absent"),
        (2, retired, retired_goal, "agent_absent"),
        (3, halted, halted_goal, "agent_halted"),
        (4, stale, older, "stale_goal"),
        (5, goalless, newer, "stale_goal"),
        (6, issued, issued_goal, "turn_open"),
        (7, spawned, spawned_goal, "turn_open"),
    ] {
        let (head, response) = post_renew(
            addr,
            &cookie,
            &csrf,
            &renew_body(&request_id(k), agent, goal, SHOWN),
        );
        refused(&head, &response, reason);
    }
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let (head, response) = post_renew(
        addr,
        &cookie,
        &csrf,
        &renew_body(&request_id(8), control, control_goal, SHOWN),
    );
    renewed(&head, &response);
    let c = renewals(&fixture, control)[0].0;
    assert_eq!(
        decisions_after(&fixture, control, c),
        [LoopDecision::Hold {
            reason: dudes_proto::HoldReason::Paused
        }],
        "the renewal decides Hold{{Paused}}"
    );
    for agent in [issued, spawned, retired, halted, stale, goalless] {
        assert!(browser_goals(&fixture, agent).is_empty(), "{agent}");
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// One index holds both kinds of browser goal request. An applied renewal answers its own key with
/// its original `200` after a later local `goal.set` and from a fresh session after a restart; the
/// same body under a new key is `stale_goal`; a changed payload under its key, a durable change's
/// key presented to the renewal route, and the renewal's key presented to the change route are
/// each an empty `409`. Nothing is appended.
#[test]
fn browser_renew_same_key_retry_returns_original_and_other_payloads_conflict() {
    let fixture = Fixture::new();
    let (settings, repo) = renew_workspace(&fixture);
    let (native_parts, native_host) = native_fixture_seams(&fixture, fake_seams(&settings));
    let daemon = boot(&settings, native_parts).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (agent, goal) = spent(&native, &repo, Usd::from_cents(1), true);
    let (changing, changing_goal) = paused_with_goal(&running, &fixture);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = renew_body(&request_id(1), agent, goal, SHOWN);
    let (head, original) = post_renew(addr, &cookie, &csrf, &body);
    renewed(&head, &original);
    let (head, response) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_body(&request_id(2), changing, changing_goal, NEW_TEXT),
    );
    changed(&head, &response);
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[true_program()]),
    );
    let later = fixture.records();

    let mut replayed = body.clone();
    replayed["request_id"] = json!(request_id(3));
    let (head, response) = post_renew(addr, &cookie, &csrf, &replayed);
    refused(&head, &response, "stale_goal");
    let conflicts = |addr: SocketAddr, cookie: &str, csrf: &str| {
        let mut changed_text = body.clone();
        changed_text["text"] = json!("another text");
        let mut change_key = body.clone();
        change_key["request_id"] = json!(request_id(2));
        let renewal_key = goal_body(&request_id(1), agent, goal, RENEWED_TEXT);
        for (label, route, payload) in [
            ("a changed payload", RENEW_ROUTE, changed_text),
            ("a durable change's key", RENEW_ROUTE, change_key),
            (
                "the renewal's key on the change route",
                GOAL_ROUTE,
                renewal_key,
            ),
        ] {
            let (head, response) = post_bytes(
                addr,
                route,
                cookie,
                csrf,
                &serde_json::to_vec(&payload).unwrap(),
                None,
            );
            assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{label}: {head}");
            assert!(response.is_null(), "{label}: {response}");
        }
    };
    let (head, response) = post_renew(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(response, original, "after a later local goal.set");
    conflicts(addr, &cookie, &csrf);
    assert_eq!(fixture.records(), later, "no retry appended anything");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let daemon = boot(&settings, fake_seams(&settings)).expect("restart");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let before = fixture.records();
    let (head, response) = post_renew(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(response, original, "from a fresh session after a restart");
    conflicts(addr, &cookie, &csrf);
    assert_eq!(fixture.records(), before);
    assert_eq!(renewals(&fixture, agent).len(), 1);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// While a renewal is in flight past its world check and before its append permit, a same-key
/// follower gets `202 pending` and a changed payload an empty `409`, both without the writer; then
/// the owner's `200`. Two keys sent from one page view yield one record and one `stale_goal`.
#[test]
fn browser_renew_concurrent_same_key_is_pending_and_two_keys_yield_one_record() {
    let fixture = Fixture::new();
    let (settings, repo) = renew_workspace(&fixture);
    let (native_parts, native_host) = native_fixture_seams(&fixture, fake_seams(&settings));
    let daemon = boot(&settings, native_parts).expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (agent, goal) = spent(&native, &repo, Usd::from_cents(1), true);
    let (second, second_goal) = spent(&native, &repo, Usd::from_cents(1), true);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = renew_body(&request_id(1), agent, goal, SHOWN);

    pause.arm(OnboardingPausePoint::BeforeGoalChangePermit);
    let owner = {
        let (cookie, csrf, body) = (cookie.clone(), csrf.clone(), body.clone());
        std::thread::spawn(move || post_renew(addr, &cookie, &csrf, &body))
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the owner is in flight"
    );
    let at_once = |body: Value| {
        let (tx, rx) = mpsc::channel();
        let (cookie, csrf) = (cookie.clone(), csrf.clone());
        std::thread::spawn(move || {
            let _ = tx.send(post_renew(addr, &cookie, &csrf, &body));
        });
        rx.recv_timeout(Duration::from_secs(3)).ok()
    };
    let follower = at_once(body.clone());
    let mut changed_text = body.clone();
    changed_text["text"] = json!("another text");
    let conflicting = at_once(changed_text);
    pause.release();
    let (head, pending) = follower.expect("a same-key follower answers without the writer");
    assert!(head.starts_with("HTTP/1.1 202 Accepted"), "{head}");
    assert_eq!(
        pending,
        json!({ "request_id": request_id(1), "state": "pending" })
    );
    let (head, response) = conflicting.expect("a changed payload answers without the writer");
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    let (head, response) = owner.join().expect("owner");
    renewed(&head, &response);
    assert_eq!(renewals(&fixture, agent).len(), 1);

    let view = renew_body(&request_id(2), second, second_goal, SHOWN);
    let mut twin = view.clone();
    twin["request_id"] = json!(request_id(3));
    let (head, response) = post_renew(addr, &cookie, &csrf, &view);
    renewed(&head, &response);
    let (head, response) = post_renew(addr, &cookie, &csrf, &twin);
    refused(&head, &response, "stale_goal");
    assert_eq!(renewals(&fixture, second).len(), 1, "one record");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// A local `goal.set` that reaches the writer first is never overwritten. One that lands between
/// the page's view and the renewal makes it `stale_goal`; and a renewal whose append failed before
/// its fsync, then a local `goal.set`, then a same-key retry, is refused.
#[test]
fn browser_renew_local_winner_is_never_overwritten() {
    let fixture = Fixture::new();
    let (settings, repo) = renew_workspace(&fixture);
    let (native_parts, native_host) = native_fixture_seams(&fixture, fake_seams(&settings));
    let daemon = boot(&settings, native_parts).expect("boot Floor");
    let append_failure = daemon.append_failure();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (moved, seen) = spent(&native, &repo, Usd::from_cents(1), true);
    let (crashed, crashed_goal) = spent(&native, &repo, Usd::from_cents(1), true);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);

    let local = common::set_goal(
        &running.handle,
        common::goal_params(moved, &[true_program()]),
    );
    let before = fixture.records();
    let (head, response) = post_renew(
        addr,
        &cookie,
        &csrf,
        &renew_body(&request_id(1), moved, seen, SHOWN),
    );
    refused(&head, &response, "stale_goal");
    assert_eq!(fixture.records(), before, "the local goal stands");
    assert_eq!(folded(&fixture).agents[&moved].goal, Some(local));

    append_failure.fail_nth(NonZeroUsize::new(1).unwrap());
    let body = renew_body(&request_id(2), crashed, crashed_goal, SHOWN);
    let (head, response) = post_renew(addr, &cookie, &csrf, &body);
    empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
    assert!(browser_goals(&fixture, crashed).is_empty());
    let local = common::set_goal(
        &running.handle,
        common::goal_params(crashed, &[true_program()]),
    );
    let (head, response) = post_renew(addr, &cookie, &csrf, &body);
    refused(&head, &response, "stale_goal");
    assert!(browser_goals(&fixture, crashed).is_empty());
    assert_eq!(folded(&fixture).agents[&crashed].goal, Some(local));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// A correctly identified live session passes the world and workspace checks, is revoked before
/// its append permit, and gets the uniform empty `403` with no `GoalSet`, `LoopDecided` or
/// `TurnIssued`. In the opposite order, revocation waits for the held permit's fsync and the
/// renewal gets its `200`.
#[test]
fn browser_renew_revocation_before_the_permit_is_forbidden() {
    let fixture = Fixture::new();
    let (settings, repo) = renew_workspace(&fixture);
    let (native_parts, native_host) = native_fixture_seams(&fixture, fake_seams(&settings));
    let daemon = boot(&settings, native_parts).expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let authorizer = daemon.floor_session_authorizer().expect("Floor sessions");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (agent, goal) = spent(&native, &repo, Usd::from_cents(1), true);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = renew_body(&request_id(1), agent, goal, SHOWN);
    let before = fixture.records();

    pause.arm(OnboardingPausePoint::BeforeGoalChangePermit);
    let losing = {
        let (cookie, csrf, body) = (cookie.clone(), csrf.clone(), body.clone());
        std::thread::spawn(move || post_renew(addr, &cookie, &csrf, &body))
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "admitted, checked, before the permit"
    );
    assert!(
        authorizer.revoke_for_test(&cookie),
        "the live session is revoked"
    );
    pause.release();
    let (head, response) = losing.join().expect("revoked request");
    empty_refusal(&head, &response, "HTTP/1.1 403 Forbidden");
    let after_revoke = fixture.records();
    assert!(
        !after_revoke[before.len()..].iter().any(|record| matches!(
            record,
            Record::GoalSet { .. } | Record::LoopDecided { .. } | Record::TurnIssued { .. }
        )),
        "{:?}",
        &after_revoke[before.len()..]
    );

    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    pause.arm(OnboardingPausePoint::AfterGoalChangePermit);
    let owner = {
        let (cookie, csrf) = (cookie.clone(), csrf.clone());
        std::thread::spawn(move || post_renew(addr, &cookie, &csrf, &body))
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the append holds its permit"
    );
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let revoke = authorizer.clone();
    let revoker = std::thread::spawn(move || {
        started_tx.send(()).expect("started signal");
        done_tx
            .send(revoke.revoke_for_test(&cookie))
            .expect("revoke result");
    });
    started_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("revoker started");
    assert!(
        done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "revocation waits while the append permit is held"
    );
    pause.release();
    assert!(
        done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("revocation completed")
    );
    revoker.join().expect("revoker thread");
    let (head, response) = owner.join().expect("winning request");
    renewed(&head, &response);
    let _ = csrf;
    assert_eq!(renewals(&fixture, agent).len(), 1);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// Session A holds its renewal's append permit, and so the registry lock, while session B's
/// renewal waits for admission and the test clock crosses the absolute expiry. A's held permit
/// wins; B gets the uniform empty `403` and appends nothing; and no clock read happens while A holds
/// the lock.
#[test]
fn browser_renew_expiry_waiting_on_registry_lock_is_forbidden() {
    let fixture = Fixture::new();
    let (settings, repo) = renew_workspace(&fixture);
    let clock = Arc::new(ProbeClock {
        inner: TestClock::ticking(1_800_000_000_000, 0),
        armed: Mutex::new(None),
    });
    let (native_parts, native_host) = native_fixture_seams(
        &fixture,
        Seams {
            host: SharedHost::new(),
            pty_host: Box::new(duded::ports::ClosedPtyHost),
            predicates: Arc::new(ScriptedRunner::default()),
            clock: Arc::clone(&clock),
            rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
            resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
            adapters: [Box::new(StubAdapter) as Box<dyn Adapter>]
                .into_iter()
                .collect(),
            children: ChildEnds::new(),
            witness: Witness::default(),
        },
    );
    let daemon = boot(&settings, native_parts).expect("boot Floor");
    let pause = daemon.onboarding_pause();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (a, a_goal) = spent(&native, &repo, Usd::from_cents(1), true);
    let (b, b_goal) = spent(&native, &repo, Usd::from_cents(1), true);
    let (a_cookie, a_csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (b_cookie, b_csrf) = exchange(addr, &issue(&settings).bootstrap);

    pause.arm(OnboardingPausePoint::AfterGoalChangePermit);
    let holder = std::thread::spawn(move || {
        post_renew(
            addr,
            &a_cookie,
            &a_csrf,
            &renew_body(&request_id(1), a, a_goal, SHOWN),
        )
    });
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "A holds its append permit"
    );
    let probe = clock.arm();
    let (sent_tx, sent_rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let body = serde_json::to_vec(&renew_body(&request_id(2), b, b_goal, SHOWN)).unwrap();
        post_bytes(addr, RENEW_ROUTE, &b_cookie, &b_csrf, &body, Some(sent_tx))
    });
    sent_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("B is sent while A holds the lock");
    let sampled_while_held = probe.recv_timeout(Duration::from_millis(500)).is_ok();
    clock.inner.advance(8 * 60 * 60 * 1000);
    pause.release();
    let (head, winner) = holder.join().expect("A");
    renewed(&head, &winner);
    let (head, response) = waiter.join().expect("B");
    assert!(
        probe.recv_timeout(Duration::from_secs(2)).is_ok() || sampled_while_held,
        "positive control: the clock was read once armed"
    );
    assert!(
        !sampled_while_held,
        "session validity was sampled while another session held the registry lock"
    );
    empty_refusal(&head, &response, "HTTP/1.1 403 Forbidden");
    assert_eq!(renewals(&fixture, a).len(), 1);
    assert!(
        browser_goals(&fixture, b).is_empty(),
        "the expired waiter appended nothing"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// An uncertain local write fences the daemon: a durable renewal's same-key retry is still served
/// from the index, with its original result and a changed payload's conflict, while a fresh
/// renewal gets an empty `503`, appends nothing, and releases its key.
#[test]
fn browser_renew_uncertain_local_write_fences_fresh_requests_and_serves_durable_retries() {
    let fixture = Fixture::new();
    let (settings, repo) = renew_workspace(&fixture);
    let (native_parts, native_host) = native_fixture_seams(&fixture, fake_seams(&settings));
    let daemon = boot(&settings, native_parts).expect("boot Floor");
    let failure = daemon.onboarding_failure();
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (first, first_goal) = spent(&native, &repo, Usd::from_cents(1), true);
    let (second, second_goal) = spent(&native, &repo, Usd::from_cents(1), true);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let durable = renew_body(&request_id(1), first, first_goal, SHOWN);
    let (head, original) = post_renew(addr, &cookie, &csrf, &durable);
    renewed(&head, &original);

    failure.arm(OnboardingFailurePoint::LocalGoal);
    let local = running
        .handle
        .ask(Ask::SetGoal(Box::new(common::goal_params(
            first,
            &[true_program()],
        ))));
    assert!(
        local.is_err(),
        "the local goal is durable and then fails its fold"
    );
    let fenced = fixture.records();

    for attempt in 0..2 {
        let (head, retry) = post_renew(addr, &cookie, &csrf, &durable);
        assert!(
            head.starts_with("HTTP/1.1 200 OK"),
            "retry {attempt} while fenced: {head}"
        );
        assert_eq!(retry, original);
    }
    let mut changed_text = durable.clone();
    changed_text["text"] = json!("another text");
    let (head, response) = post_renew(addr, &cookie, &csrf, &changed_text);
    empty_refusal(&head, &response, "HTTP/1.1 409 Conflict");
    let fresh = renew_body(&request_id(2), second, second_goal, SHOWN);
    for _ in 0..2 {
        let (head, response) = post_renew(addr, &cookie, &csrf, &fresh);
        empty_refusal(&head, &response, "HTTP/1.1 503 Service Unavailable");
    }
    assert_eq!(fixture.records(), fenced, "nothing appended while fenced");
    assert!(browser_goals(&fixture, second).is_empty());
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The body's bounds refuse before any effect: the change's text, member and identity bounds, a
/// missing ceiling member (which must be present, as a string or `null`), and each allowance member
/// out of range or not in canonical decimal text, plus a 16,385-byte body (`413`, before a byte is
/// read). Control: 2,048 bytes of U+0001, which `JSON.stringify` escapes, applies.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "every bound is one row of a single table beside its control"
)]
fn browser_renew_bounds_and_json_shape_refuse_before_effect() {
    let fixture = Fixture::new();
    let (settings, repo) = renew_workspace(&fixture);
    let (native_parts, native_host) = native_fixture_seams(&fixture, fake_seams(&settings));
    let daemon = boot(&settings, native_parts).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (agent, goal) = spent(&native, &repo, Usd::from_cents(1), true);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let id = request_id(1);
    let alias = agent.to_string().replacen('0', "O", 1);
    assert_ne!(alias, agent.to_string(), "the alias spelling differs");
    let head_members = format!(r#""request_id":"{id}","agent":"{agent}","goal":"{goal}""#);
    let shown = r#""hop_cap":"3","goal_usd_micros":"1000000","goal_tokens":null"#;
    let with = |text: &str, numbers: &str| format!("{{{head_members},\"text\":{text},{numbers}}}");
    let over = format!("\"{}\"", "x".repeat(2049));
    let refusals: Vec<(&str, String)> = vec![
        ("2,049 bytes of text", with(&over, shown)),
        ("nothing after trim", with(r#"" \t\r\n""#, shown)),
        ("U+0000", with(r#""a\u0000b""#, shown)),
        ("a lone surrogate", with(r#""a\ud800b""#, shown)),
        (
            "unknown member",
            with(r#""t""#, &format!("{shown},\"predicate\":1")),
        ),
        (
            "duplicate member",
            with(r#""t""#, &format!("{shown},\"hop_cap\":\"3\"")),
        ),
        ("nested member", with(r#"{"t":1}"#, shown)),
        (
            "missing goal_tokens",
            with(r#""t""#, r#""hop_cap":"3","goal_usd_micros":"1000000""#),
        ),
        (
            "missing goal_usd_micros",
            with(r#""t""#, r#""hop_cap":"3","goal_tokens":null"#),
        ),
        (
            "missing hop_cap",
            with(
                r#""t""#,
                r#""goal_usd_micros":"1000000","goal_tokens":null"#,
            ),
        ),
        (
            "a zero hop cap",
            with(
                r#""t""#,
                r#""hop_cap":"0","goal_usd_micros":"1000000","goal_tokens":null"#,
            ),
        ),
        (
            "a hop cap past u32",
            with(
                r#""t""#,
                r#""hop_cap":"4294967296","goal_usd_micros":"1000000","goal_tokens":null"#,
            ),
        ),
        (
            "a leading zero",
            with(
                r#""t""#,
                r#""hop_cap":"03","goal_usd_micros":"1000000","goal_tokens":null"#,
            ),
        ),
        (
            "a plus sign",
            with(
                r#""t""#,
                r#""hop_cap":"+3","goal_usd_micros":"1000000","goal_tokens":null"#,
            ),
        ),
        (
            "a JSON number",
            with(
                r#""t""#,
                r#""hop_cap":3,"goal_usd_micros":"1000000","goal_tokens":null"#,
            ),
        ),
        (
            "a ceiling past u64",
            with(
                r#""t""#,
                r#""hop_cap":"3","goal_usd_micros":"18446744073709551616","goal_tokens":null"#,
            ),
        ),
        (
            "a ceiling as a number",
            with(
                r#""t""#,
                r#""hop_cap":"3","goal_usd_micros":1000000,"goal_tokens":null"#,
            ),
        ),
        (
            "a ceiling with a fraction",
            with(
                r#""t""#,
                r#""hop_cap":"3","goal_usd_micros":"1.5","goal_tokens":null"#,
            ),
        ),
        (
            "a ceiling of the empty string",
            with(
                r#""t""#,
                r#""hop_cap":"3","goal_usd_micros":"1000000","goal_tokens":"""#,
            ),
        ),
        (
            "noncanonical request_id",
            format!(
                r#"{{"request_id":"{}B","agent":"{agent}","goal":"{goal}","text":"t",{shown}}}"#,
                "A".repeat(42)
            ),
        ),
        (
            "alias-letter agent",
            format!(
                r#"{{"request_id":"{id}","agent":"{alias}","goal":"{goal}","text":"t",{shown}}}"#
            ),
        ),
        (
            "an array",
            format!(r#"["{id}","{agent}","{goal}","t","3",null,null]"#),
        ),
    ];
    let before = fixture.records();
    for (label, body) in &refusals {
        let (head, response) = post_bytes(addr, RENEW_ROUTE, &cookie, &csrf, body.as_bytes(), None);
        assert!(
            head.starts_with("HTTP/1.1 400 Bad Request"),
            "{label}: {head}"
        );
        assert!(response.is_null(), "{label}: {response}");
    }
    let host = addr.to_string();
    let oversize = format!(
        "POST {RENEW_ROUTE} HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: dudes_floor_session={cookie}\r\nX-Dudes-CSRF: {csrf}\r\nContent-Type: application/json\r\nContent-Length: 16385\r\n\r\n"
    );
    let (head, body) = raw_onboard(addr, oversize.as_bytes());
    assert!(
        head.starts_with("HTTP/1.1 413"),
        "refused before a byte is read: {head}"
    );
    assert!(body.is_empty());
    assert_eq!(fixture.records(), before, "no refusal moved the journal");

    let mut widest = renew_body(&request_id(2), agent, goal, SHOWN);
    widest["text"] = json!("\u{1}".repeat(2048));
    let widest = serde_json::to_vec(&widest).unwrap();
    assert!(widest.len() < 16_384, "{}", widest.len());
    let (head, response) = post_bytes(addr, RENEW_ROUTE, &cookie, &csrf, &widest, None);
    renewed(&head, &response);
    let (_, new_goal, _) = renewals(&fixture, agent).pop().expect("applied");
    assert_eq!(new_goal.text, "\u{1}".repeat(2048));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// On an attached agent the renewal applies, the control gate holds it, and no `pty.write` or `Steer`
/// carries the text.
#[test]
fn browser_renew_on_attached_agent_writes_no_pty() {
    let fixture = Fixture::new();
    let (settings, repo) = renew_workspace(&fixture);
    let (native_parts, native_host) = native_fixture_seams(&fixture, fake_seams(&settings));
    let mut running = common::start(boot(&settings, native_parts).expect("boot the setup run"));
    let native = NativeTurnFixture {
        running: &running,
        fixture: &fixture,
        host: &native_host,
    };
    let (agent, goal) = spent(&native, &repo, Usd::from_cents(1), true);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    seed(
        &fixture,
        [Record::Attached {
            agent,
            by: dudes_proto::Actor::System,
            hook_port: None,
            provenance: None,
        }],
    );
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    running
        .handle
        .ask(Ask::Resume(agent))
        .expect("local resume");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, response) = post_renew(
        addr,
        &cookie,
        &csrf,
        &renew_body(&request_id(1), agent, goal, SHOWN),
    );
    renewed(&head, &response);
    let c = renewals(&fixture, agent)[0].0;
    let _ = snapshot(addr, &cookie, &csrf);
    let renewed_records = after(&fixture, c);
    assert!(
        !renewed_records.iter().any(|(_, e)| matches!(
            &e.record,
            Record::TurnIssued { .. }
                | Record::PtyWriteRequested { .. }
                | Record::Steer { .. }
                | Record::Detached { .. }
        )),
        "the control gate holds the attached agent and nothing types the text: {renewed_records:?}"
    );
    assert!(
        matches!(
            decisions_after(&fixture, agent, c).first(),
            Some(LoopDecision::Hold { .. })
        ),
        "the renewal's decision is a Hold"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// With renewal off, goal change on, the route does not exist: its response is byte for byte an
/// unrouted POST's, and the `goal` sibling carries no `renew` member. Control: renewal on routes it
/// and sends the member. The switch is refused at load without both goal change and Hire.
#[test]
fn browser_renew_is_absent_when_not_enabled() {
    for enabled in [false, true] {
        let fixture = Fixture::new();
        let (mut settings, repo) = renew_workspace(&fixture);
        if !enabled {
            settings.floor_control = FloorControl::On {
                goal: true,
                retire: false,
                renew: false,
            };
        }
        let (native_parts, native_host) = native_fixture_seams(&fixture, fake_seams(&settings));
        let daemon = boot(&settings, native_parts).expect("boot Floor");
        let addr = daemon.floor_addr().expect("Floor address");
        let mut running = common::start(daemon);
        let native = NativeTurnFixture {
            running: &running,
            fixture: &fixture,
            host: &native_host,
        };
        let (agent, goal) = spent(&native, &repo, Usd::from_cents(1), true);
        let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
        let (view, _) = snapshot(addr, &cookie, &csrf);
        assert_eq!(
            view["settings"]["switches"]["renew"],
            json!(enabled),
            "{view}"
        );
        let body = serde_json::to_vec(&renew_body(&request_id(1), agent, goal, SHOWN)).unwrap();
        let unrouted = raw_post(addr, "/floor/unrouted", &cookie, &csrf, &body);
        assert!(!unrouted.is_empty());
        let routed = raw_post(addr, RENEW_ROUTE, &cookie, &csrf, &body);
        if enabled {
            assert!(
                String::from_utf8_lossy(&routed).starts_with("HTTP/1.1 200 OK"),
                "control: {}",
                String::from_utf8_lossy(&routed)
            );
            assert!(view["goal"]["renew"].is_object(), "{view}");
            assert_eq!(renewals(&fixture, agent).len(), 1);
        } else {
            assert_eq!(
                routed,
                unrouted,
                "disabled is exactly unrouted: {}",
                String::from_utf8_lossy(&routed)
            );
            assert_eq!(view["goal"], json!({ "max_text_bytes": 2048 }));
            assert!(browser_goals(&fixture, agent).is_empty());
        }
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }

    let root = tempfile::tempdir().expect("operator config root");
    let config = root.path().join("config.toml");
    let load = |source: &str| {
        std::fs::write(&config, source).expect("config");
        load_operator(&config).map(|operator| operator.control)
    };
    let control = "[floor.control]\nenabled = true\n";
    let goal = "[floor.goal]\nenabled = true\n";
    let hire = "[floor.hire]\nenabled = true\n";
    let renew = "[floor.renew]\nenabled = true\n";
    assert_eq!(
        load(&format!("{control}{goal}{hire}{renew}")).expect("control: goal change and Hire"),
        FloorControl::On {
            goal: true,
            retire: false,
            renew: true,
        }
    );
    assert_eq!(
        load(&format!("{control}{goal}{hire}")).expect("absent"),
        FloorControl::On {
            goal: true,
            retire: false,
            renew: false,
        },
        "absent is off"
    );
    for (label, source) in [
        ("no [floor.goal]", format!("{control}{hire}{renew}")),
        ("no [floor.hire]", format!("{control}{goal}{renew}")),
        ("neither", renew.to_owned()),
    ] {
        let error = load(&source).expect_err(label).to_string();
        assert!(
            error.ends_with(
                "enables [floor.renew] without both [floor.goal] and [floor.hire] enabled"
            ),
            "{label}: {error}"
        );
    }
}

// --- A goal's check runs once first (Floor routes) --------------------------------------------

/// A predicate runner that answers each check by the last argument of its command, failing any it
/// was not told about, and records the order the checks ran in. It can hold the next run, and
/// only that one, until the test releases it.
#[derive(Default)]
struct BaselineRunner {
    answers: Mutex<std::collections::HashMap<String, duded::predicate::PredicateOutcome>>,
    ran: Mutex<Vec<String>>,
    hold_next: std::sync::atomic::AtomicBool,
    closed: Mutex<bool>,
    opened: std::sync::Condvar,
}

impl BaselineRunner {
    fn new() -> Arc<Self> {
        Arc::default()
    }

    fn answer(&self, key: &str, outcome: duded::predicate::PredicateOutcome) {
        self.answers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_owned(), outcome);
    }

    fn ran(&self) -> Vec<String> {
        self.ran
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn hold_next(&self) {
        *self.closed.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.hold_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn release(&self) {
        *self.closed.lock().unwrap_or_else(PoisonError::into_inner) = false;
        self.opened.notify_all();
    }

    #[track_caller]
    fn wait_runs(&self, runs: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while self.ran().len() < runs {
            assert!(
                std::time::Instant::now() < deadline,
                "only {:?} ran",
                self.ran()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl duded::predicate::PredicateRunner for BaselineRunner {
    fn run(
        &self,
        spec: &LaunchSpec,
        _timeout: Duration,
    ) -> std::io::Result<duded::predicate::PredicateOutcome> {
        let key = spec
            .argv
            .last()
            .and_then(|arg| arg.to_str())
            .unwrap_or_default()
            .to_owned();
        let outcome = self
            .answers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .cloned()
            .unwrap_or_else(common::fails);
        let hold = self
            .hold_next
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        self.ran
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(key);
        if hold {
            let mut closed = self.closed.lock().unwrap_or_else(PoisonError::into_inner);
            while *closed {
                closed = self
                    .opened
                    .wait(closed)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        Ok(outcome)
    }

    fn sweep(&self) {
        self.release();
    }
}

/// [`checks_settings`]' checks: id, label and whether the operator says it may already pass. Each
/// check's command ends in its id, which is how [`BaselineRunner`] tells them apart.
const CHECKS: [(&str, &str, bool); 3] = [
    ("default", "Checks pass", false),
    ("contributing", "CONTRIBUTING.md exists", false),
    ("regression", "Suite still passes", true),
];

/// Hire and goal change on, with three checks enrolled for the workspace, its own first.
fn checks_settings(fixture: &Fixture) -> duded::Settings {
    let mut settings = enrolled_settings(fixture);
    settings.floor_control = FloorControl::On {
        goal: true,
        retire: false,
        renew: false,
    };
    settings.enrollment.profiles[0].checks = CHECKS
        .iter()
        .map(
            |&(id, label, may_already_pass)| duded::floor_actions::EnrolledCheck {
                id: id.to_owned(),
                label: label.to_owned(),
                predicate: Predicate::Command {
                    argv: vec![true_program().to_owned(), id.to_owned()],
                    timeout_s: 10,
                },
                may_already_pass,
            },
        )
        .collect();
    settings
}

/// The revision the `checks` sibling shows for one of [`CHECKS`], recomputed here.
fn enrolled_revision(id: &str) -> String {
    let &(_, label, may_already_pass) = CHECKS
        .iter()
        .find(|check| check.0 == id)
        .expect("an enrolled check");
    common::check_revision_as(id, label, &[true_program(), id], 10, may_already_pass)
}

/// A new hire body naming one of [`CHECKS`].
fn hire_naming(attempt: &str, check: &str) -> Value {
    json!({
        "attempt_id": attempt, "workspace_id": "repo_1", "name": "Wanda", "role": "writer",
        "goal_text": "Write CONTRIBUTING.md", "check_id": check,
        "check_revision": enrolled_revision(check)
    })
}

/// A goal change body naming one of [`CHECKS`].
fn goal_naming(request_id: &str, agent: AgentId, goal: GoalId, check: &str) -> Value {
    let mut body = goal_body(request_id, agent, goal, NEW_TEXT);
    body["check_id"] = json!(check);
    body["check_revision"] = json!(enrolled_revision(check));
    body
}

/// A canonical attempt id, distinct for each `n` below 1000.
fn attempt(n: u16) -> String {
    format!("{n:03}{}", "A".repeat(40))
}

fn hires(fixture: &Fixture) -> Vec<BrowserOnboardingHire> {
    fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::AgentHired {
                browser_onboarding: Some(hire),
                ..
            } => Some(*hire),
            _ => None,
        })
        .collect()
}

/// The reason of a `409 refused` answer.
#[track_caller]
fn refused_reason(head: &str, body: &Value) -> String {
    assert!(head.starts_with("HTTP/1.1 409 Conflict"), "{head} {body}");
    assert_eq!(body["state"], "refused", "{body}");
    body["reason"].as_str().expect("a reason").to_owned()
}

/// `goal.set` with a check that must run first, retried while the agent's turn or post-turn
/// check is still running.
fn set_goal_once_idle(handle: &duded::Handle, agent: AgentId, key: &str) -> GoalId {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match handle.set_goal(
            GoalSetParams {
                may_already_pass: false,
                ..common::goal_params(agent, &[true_program(), key])
            },
            &dudes_rpc::HangUp::never(),
        ) {
            Ok(set) => return set.goal,
            Err(error) if error.message.contains("has a turn or a check running") => {
                assert!(std::time::Instant::now() < deadline, "{error:?}");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("goal.set refused: {error:?}"),
        }
    }
}

/// A hire whose check already passes waits for that run and is refused: nobody is hired, and a
/// same-key follower that arrives during the run gets `202 pending`. A hire naming a check that
/// fails completes.
///
/// Mutation: hiring without waiting for the run hires an agent whose check already passes.
#[test]
fn browser_onboard_refuses_a_passing_check_before_hire() {
    let fixture = Fixture::new();
    let settings = checks_settings(&fixture);
    let runner = BaselineRunner::new();
    runner.answer("default", common::passes());
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    runner.hold_next();
    let body = hire_naming(&attempt(0), "default");
    let owner = {
        let (cookie, csrf, body) = (cookie.clone(), csrf.clone(), body.clone());
        std::thread::spawn(move || post_onboard(addr, &cookie, &csrf, &body))
    };
    runner.wait_runs(1);
    let (head, follower) = post_onboard(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 202 Accepted"), "{head}");
    assert_eq!(follower["state"], "pending");
    runner.release();
    let (head, refused) = owner.join().expect("the owner's answer");
    assert_eq!(refused_reason(&head, &refused), "check_already_passes");
    assert!(hires(&fixture).is_empty(), "nobody was hired");

    let (head, complete) = post_onboard(
        addr,
        &cookie,
        &csrf,
        &hire_naming(&attempt(1), "contributing"),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {complete}");
    assert_eq!(runner.ran(), ["default", "contributing"]);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// A hire records the check it names, with that check's own revision and what its run saw, and
/// the request digest binds the check. A revision the page did not read is `profile_changed`,
/// and a new attempt that names no check is malformed; neither runs anything.
///
/// Mutation: recording the workspace's first check records the wrong name.
#[test]
fn browser_onboard_records_the_chosen_check() {
    let fixture = Fixture::new();
    let settings = checks_settings(&fixture);
    let runner = BaselineRunner::new();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let mut stale = hire_naming(&attempt(0), "contributing");
    stale["check_revision"] = json!(enrolled_revision("default"));
    let (head, refused) = post_onboard(addr, &cookie, &csrf, &stale);
    assert_eq!(refused_reason(&head, &refused), "profile_changed");
    let mut nameless = hire_naming(&attempt(1), "contributing");
    nameless["check_id"] = Value::Null;
    let (head, _) = post_onboard(addr, &cookie, &csrf, &nameless);
    assert!(head.starts_with("HTTP/1.1 400 Bad Request"), "{head}");
    assert!(runner.ran().is_empty(), "neither ran a check");

    let (head, complete) = post_onboard(
        addr,
        &cookie,
        &csrf,
        &hire_naming(&attempt(2), "contributing"),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {complete}");
    let [hire] = <[_; 1]>::try_from(hires(&fixture)).expect("one hire");
    let intended = &hire.intended_goal;
    let check = intended.completion_check.as_ref().expect("a check name");
    assert_eq!(
        (check.id.as_str(), check.label.as_str()),
        ("contributing", "CONTRIBUTING.md exists")
    );
    assert_eq!(
        serde_json::to_value(check.revision).unwrap(),
        json!(enrolled_revision("contributing"))
    );
    let program = std::fs::canonicalize(true_program())
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        intended.baseline,
        Some(dudes_proto::CheckBaseline::CommandFailed { program, exit: 1 })
    );
    let goal = fixture
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::GoalSet {
                goal,
                browser_onboarding: Some(_),
                ..
            } => Some(goal),
            _ => None,
        })
        .expect("the hire's goal");
    assert_eq!(&goal, intended, "the goal is the intended one");
    let digest = framed(
        b"dudes.onboard.v4\0",
        &[
            ("workspace_id", "repo_1"),
            ("name", "Wanda"),
            ("role", "writer"),
            ("goal_text", "Write CONTRIBUTING.md"),
            ("provider", "claude"),
            ("character", ""),
            ("accent", ""),
            ("check_id", "contributing"),
            ("check_revision", &enrolled_revision("contributing")),
        ],
    );
    assert_eq!(hire.request_digest, digest, "the digest binds the check");
    assert_eq!(runner.ran(), ["contributing"]);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// While a local goal set's check runs for a durable hire's agent, the hire's retry runs no
/// check beside it and stays pending, because the hire is durable. The local check passes, so it
/// sets no goal.
fn retry_beside_a_local_check(
    running: &common::Running,
    runner: &BaselineRunner,
    fixture: &Fixture,
    (addr, cookie, csrf): (SocketAddr, &str, &str),
    body: &Value,
) {
    let [hire] = <[_; 1]>::try_from(hires(fixture)).expect("one hire");
    let agent = fixture
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::AgentHired {
                id,
                browser_onboarding: Some(meta),
                ..
            } if meta.attempt_id == hire.attempt_id => Some(id),
            _ => None,
        })
        .expect("the hired agent");
    runner.answer("local", common::passes());
    runner.hold_next();
    let local = {
        let handle = running.handle.clone();
        std::thread::spawn(move || {
            handle.set_goal(
                GoalSetParams {
                    may_already_pass: false,
                    ..common::goal_params(agent, &[true_program(), "local"])
                },
                &dudes_rpc::HangUp::never(),
            )
        })
    };
    runner.wait_runs(1);
    let (head, _) = post_onboard(addr, cookie, csrf, body);
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    runner.release();
    local
        .join()
        .unwrap()
        .expect_err("the local check already passes, so it sets no goal");
}

/// A partial attempt's retry runs its check again before the hire's goal is appended: a check
/// that now passes leaves the agent hired without a goal, and one that fails again completes it.
/// While a local goal set's check runs for the agent, the retry stays pending and runs nothing.
///
/// Mutations: skipping the rerun appends the goal on the first retry; starting the retry's check
/// beside the local one answers it refused instead of pending.
#[test]
fn browser_onboard_partial_retry_reruns_the_baseline() {
    let fixture = Fixture::new();
    let settings = checks_settings(&fixture);
    let clock = Arc::new(TestClock::ticking(1_800_000_000_000, 0));
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            BaselineRunner::new(),
            Arc::clone(&clock),
            Witness::default(),
        ),
    )
    .expect("first boot");
    let pause = daemon.onboarding_pause();
    pause.arm(OnboardingPausePoint::BeforeGoalPermit);
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = hire_naming(&attempt(0), "contributing");
    let owner = {
        let body = body.clone();
        std::thread::spawn(move || post_onboard(addr, &cookie, &csrf, &body))
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the hire is durable"
    );
    clock.advance(8 * 60 * 60 * 1000);
    pause.release();
    let (_, partial) = owner.join().expect("the partial answer");
    assert_eq!(partial["reason"], "session_expired");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let runner = BaselineRunner::new();
    runner.answer("contributing", common::passes());
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("second boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);

    retry_beside_a_local_check(&running, &runner, &fixture, (addr, &cookie, &csrf), &body);

    let (head, partial) = post_onboard(addr, &cookie, &csrf, &body);
    assert!(
        head.starts_with("HTTP/1.1 409 Conflict"),
        "{head} {partial}"
    );
    assert_eq!(partial["state"], "hired_without_goal");
    assert_eq!(partial["reason"], "goal_refused");
    assert_eq!(
        runner.ran(),
        ["local", "contributing"],
        "the retry ran the check again once the local one ended"
    );
    let onboarding_goals = |fixture: &Fixture| {
        fixture
            .records()
            .iter()
            .filter(|record| {
                matches!(
                    record,
                    Record::GoalSet {
                        browser_onboarding: Some(_),
                        ..
                    }
                )
            })
            .count()
    };
    assert_eq!(onboarding_goals(&fixture), 0, "no goal was appended");

    runner.answer("contributing", common::fails());
    let (head, complete) = post_onboard(addr, &cookie, &csrf, &body);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {complete}");
    assert_eq!(onboarding_goals(&fixture), 1);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// A goal change that keeps a check that passes is refused and changes nothing. The same change
/// naming an enrolled check that fails applies with that check's name, and a chosen or kept
/// check the operator says may already pass applies with no run.
///
/// Mutation: keeping the replaced goal's check when the request names one records the old name.
#[test]
fn browser_goal_change_keeps_or_names_a_check() {
    let fixture = Fixture::new();
    let settings = checks_settings(&fixture);
    let runner = BaselineRunner::new();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, hired) = post_onboard(
        addr,
        &cookie,
        &csrf,
        &hire_naming(&attempt(0), "regression"),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {hired}");
    let agent = AgentId::parse(hired["agent"].as_str().unwrap()).unwrap();
    await_entry(
        &fixture,
        "the first turn",
        |_, record| matches!(record, Record::TurnIssued { agent: who, .. } if *who == agent),
    );
    running.handle.ask(Ask::Pause(agent)).expect("local pause");
    end_turn(&running.handle, newest_turn(&fixture, agent), None);
    let goal = set_goal_once_idle(&running.handle, agent, "local");

    runner.answer("local", common::passes());
    let (head, refused) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_body(&request_id(1), agent, goal, NEW_TEXT),
    );
    assert_eq!(refused_reason(&head, &refused), "check_already_passes");
    assert!(browser_goals(&fixture, agent).is_empty(), "nothing changed");

    let (head, response) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_naming(&request_id(2), agent, goal, "contributing"),
    );
    let named = changed(&head, &response);
    let [(_, new_goal, attribution)] =
        <[_; 1]>::try_from(browser_goals(&fixture, agent)).expect("one change");
    assert!(attribution.check_chosen);
    assert_eq!(
        new_goal.completion_check.map(|check| check.id),
        Some("contributing".to_owned())
    );
    assert!(matches!(
        new_goal.baseline,
        Some(dudes_proto::CheckBaseline::CommandFailed { exit: 1, .. })
    ));

    let ran = runner.ran().len();
    let (head, response) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_naming(&request_id(3), agent, named, "regression"),
    );
    let chosen = changed(&head, &response);
    let (head, response) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_body(&request_id(4), agent, chosen, "keep it"),
    );
    changed(&head, &response);
    assert_eq!(runner.ran().len(), ran, "neither change ran a check");
    let goals = browser_goals(&fixture, agent);
    assert!(goals[1..].iter().all(|(_, goal, _)| {
        goal.baseline == Some(dudes_proto::CheckBaseline::MayAlreadyPass)
            && goal
                .completion_check
                .as_ref()
                .map(|check| check.id.as_str())
                == Some("regression")
    }));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

struct HeldInstructionClock {
    clock: TestClock,
    state: Mutex<(bool, bool)>,
    changed: std::sync::Condvar,
}

impl HeldInstructionClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            clock: TestClock::new(),
            state: Mutex::new((false, false)),
            changed: std::sync::Condvar::new(),
        })
    }

    fn hold(self: &Arc<Self>) -> ReleaseInstructionClock {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = (true, false);
        ReleaseInstructionClock(Arc::clone(self))
    }

    fn wait_held(&self) {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(5), |state| !state.1)
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            state.1,
            "the post-turn instruction sample reached its clock"
        );
    }
}

impl dudes_supervisor::Clock for HeldInstructionClock {
    fn now(&self) -> dudes_types::Timestamp {
        if matches!(
            std::thread::current().name(),
            Some("dudes-instructions-io" | "dudes-instructions-deadline")
        ) {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.0 {
                state.1 = true;
                self.changed.notify_all();
                while state.0 {
                    state = self
                        .changed
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
        dudes_supervisor::Clock::now(&self.clock)
    }
}

struct ReleaseInstructionClock(Arc<HeldInstructionClock>);

impl Drop for ReleaseInstructionClock {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0 = false;
        self.0.changed.notify_all();
    }
}

fn held_instruction_seams(
    settings: &duded::Settings,
    runner: &Arc<BaselineRunner>,
    clock: &Arc<HeldInstructionClock>,
) -> Seams<SharedHost, BaselineRunner, HeldInstructionClock, common::FixtureRng> {
    let parts = seams(
        settings,
        SharedHost::new(),
        Arc::clone(runner),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    Seams {
        clock: Arc::clone(clock),
        host: parts.host,
        pty_host: parts.pty_host,
        predicates: parts.predicates,
        rng: parts.rng,
        resolver: parts.resolver,
        adapters: parts.adapters,
        children: parts.children,
        witness: parts.witness,
    }
}

#[derive(Clone, Copy)]
enum PendingCheckRoute {
    CheckedSet,
    Replace,
    Change,
    Renew,
}

fn pending_check_request(
    route: PendingCheckRoute,
    running: &common::Running,
    settings: &duded::Settings,
    session: (SocketAddr, &str, &str),
    agent: AgentId,
    goal: GoalId,
) -> Option<GoalId> {
    use dudes_proto::rpc::goal::{
        CompletionCheckDeclaration, GoalReplaceParams, GoalReplaceRefusal, GoalReplaceResult,
    };
    match route {
        PendingCheckRoute::CheckedSet => match running.handle.set_goal(
            GoalSetParams {
                may_already_pass: false,
                ..common::goal_params(agent, &[true_program(), "local"])
            },
            &dudes_rpc::HangUp::never(),
        ) {
            Ok(set) => Some(set.goal),
            Err(error) => {
                assert_eq!(
                    error.message,
                    "the agent has a turn or a check running in its directory, so the completion check cannot run there now"
                );
                None
            }
        },
        PendingCheckRoute::Replace => {
            let credential = OperatorCredential::new(
                std::fs::read_to_string(
                    settings
                        .runtime_dir
                        .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
                )
                .unwrap(),
            )
            .unwrap();
            let params = GoalReplaceParams {
                request_id: Hex32::new([7; 32]),
                agent,
                expected_goal: goal,
                text: NEW_TEXT.into(),
                predicate: Predicate::Command {
                    argv: vec![true_program().into(), "local".into()],
                    timeout_s: 10,
                },
                check: CompletionCheckDeclaration {
                    id: "local".into(),
                    label: "Local check".into(),
                },
                may_already_pass: false,
            };
            let reply = Client::connect(settings.socket())
                .unwrap()
                .call_as_operator(
                    Method::GoalReplace,
                    Some(serde_json::to_value(params).unwrap()),
                    credential,
                )
                .unwrap();
            assert!(reply.error.is_none(), "{reply:?}");
            match serde_json::from_value::<GoalReplaceResult>(reply.result.unwrap()).unwrap() {
                GoalReplaceResult::Changed { goal, .. } => Some(goal),
                GoalReplaceResult::Refused { reason, .. } => {
                    assert_eq!(reason, GoalReplaceRefusal::TurnOpen);
                    None
                }
                other @ GoalReplaceResult::Pending { .. } => {
                    panic!("unexpected replacement reply: {other:?}")
                }
            }
        }
        PendingCheckRoute::Change | PendingCheckRoute::Renew => {
            let (addr, cookie, csrf) = session;
            let renewal = matches!(route, PendingCheckRoute::Renew);
            let (head, body) = if renewal {
                post_renew(
                    addr,
                    cookie,
                    csrf,
                    &renew_body(&request_id(7), agent, goal, ("1", Some("5000000"), None)),
                )
            } else {
                post_goal(
                    addr,
                    cookie,
                    csrf,
                    &goal_naming(&request_id(7), agent, goal, "contributing"),
                )
            };
            if head.starts_with("HTTP/1.1 409 Conflict") {
                assert_eq!(refused_reason(&head, &body), "turn_open");
                None
            } else if renewal {
                Some(renewed(&head, &body))
            } else {
                Some(changed(&head, &body))
            }
        }
    }
}

fn pending_instruction_check_blocks(route: PendingCheckRoute) {
    let fixture = Fixture::new();
    let mut settings = checks_settings(&fixture);
    settings.floor_control = FloorControl::On {
        goal: true,
        retire: false,
        renew: true,
    };
    if matches!(route, PendingCheckRoute::Renew) {
        settings.enrollment.profiles[0].hop_cap = 1;
    }
    let runner = BaselineRunner::new();
    let clock = HeldInstructionClock::new();
    let fixture_parts = held_instruction_seams(&settings, &runner, &clock);
    let daemon = boot(&settings, fixture_parts).expect("boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, hired) = post_onboard(
        addr,
        &cookie,
        &csrf,
        &hire_naming(&attempt(0), "regression"),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {hired}");
    let agent = AgentId::parse(hired["agent"].as_str().unwrap()).unwrap();
    let goal = fixture.await_record("the initial goal", |record| {
        record
            .goal_install()
            .filter(|(owner, _)| *owner == agent)
            .map(|(_, goal)| goal.id)
    });
    let turn = issued_goal_turn(&fixture, goal);
    running.handle.ask(Ask::Pause(agent)).expect("local pause");
    let release = clock.hold();
    end_turn(&running.handle, turn, None);
    clock.wait_held();
    running.handle.doctor().unwrap();
    let before = fixture.records();
    assert_eq!(
        pending_check_request(
            route,
            &running,
            &settings,
            (addr, &cookie, &csrf),
            agent,
            goal
        ),
        None
    );
    assert!(runner.ran().is_empty(), "admission started no baseline");
    assert_eq!(fixture.records(), before, "refusal appended nothing");
    let other = common::hire(&running.handle, fixture.state.path());
    running
        .handle
        .ask(Ask::Pause(other))
        .expect("independent pause");
    let independent = pending_check_request(
        PendingCheckRoute::CheckedSet,
        &running,
        &settings,
        (addr, &cookie, &csrf),
        other,
        goal,
    );
    assert!(
        independent.is_some(),
        "another agent can set a checked goal"
    );
    assert_eq!(runner.ran(), ["local"]);
    drop(release);
    let judged = await_entry(
        &fixture,
        "the old verdict",
        |_, record| matches!(record, Record::PredicateResult { goal: old, turn: Some(ended), .. } if *old == goal && *ended == turn),
    );
    await_entry(
        &fixture,
        "the decision after the old verdict",
        |lsn, record| {
            lsn > judged
                && matches!(record, Record::LoopDecided { agent: who, .. } if *who == agent)
        },
    );
    assert_eq!(runner.ran(), ["local", "regression"]);
    let replacement = pending_check_request(
        route,
        &running,
        &settings,
        (addr, &cookie, &csrf),
        agent,
        goal,
    );
    assert!(
        replacement.is_some(),
        "the settled check admits replacement"
    );
    assert_ne!(replacement, Some(goal));
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn checked_goal_set_waits_for_pending_instruction_check() {
    pending_instruction_check_blocks(PendingCheckRoute::CheckedSet);
}

#[test]
fn local_goal_replace_waits_for_pending_instruction_check() {
    pending_instruction_check_blocks(PendingCheckRoute::Replace);
}

#[test]
fn browser_goal_change_waits_for_pending_instruction_check() {
    pending_instruction_check_blocks(PendingCheckRoute::Change);
}

#[test]
fn browser_goal_renewal_waits_for_pending_instruction_check() {
    pending_instruction_check_blocks(PendingCheckRoute::Renew);
}

#[test]
fn legacy_goal_set_keeps_its_bypass_and_browser_current_goal_check_rules() {
    let fixture = Fixture::new();
    let settings = checks_settings(&fixture);
    let runner = BaselineRunner::new();
    let clock = HeldInstructionClock::new();
    let fixture_parts = held_instruction_seams(&settings, &runner, &clock);
    let daemon = boot(&settings, fixture_parts).expect("boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (head, hired) = post_onboard(
        addr,
        &cookie,
        &csrf,
        &hire_naming(&attempt(0), "regression"),
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head} {hired}");
    let agent = AgentId::parse(hired["agent"].as_str().unwrap()).unwrap();
    let old_goal = fixture.await_record("the initial goal", |record| {
        record
            .goal_install()
            .filter(|(owner, _)| *owner == agent)
            .map(|(_, goal)| goal.id)
    });
    let turn = issued_goal_turn(&fixture, old_goal);
    running.handle.ask(Ask::Pause(agent)).expect("local pause");
    let release = clock.hold();
    end_turn(&running.handle, turn, None);
    clock.wait_held();
    let params = common::goal_params(agent, &[true_program(), "regression"]);
    let legacy = common::set_goal(&running.handle, params);
    assert_ne!(legacy, old_goal);
    assert!(
        runner.ran().is_empty(),
        "the legacy setter starts no baseline"
    );
    let session = (addr, cookie.as_str(), csrf.as_str());
    let before = fixture.records();
    assert_eq!(
        pending_check_request(
            PendingCheckRoute::Change,
            &running,
            &settings,
            session,
            agent,
            legacy
        ),
        None
    );
    assert_eq!(
        fixture.records(),
        before,
        "the pending old continuation still belongs to this agent"
    );
    runner.hold_next();
    drop(release);
    runner.wait_runs(1);
    assert_eq!(runner.ran(), ["regression"]);
    assert_eq!(
        pending_check_request(
            PendingCheckRoute::CheckedSet,
            &running,
            &settings,
            session,
            agent,
            legacy
        ),
        None
    );
    let (head, body) = post_goal(
        addr,
        &cookie,
        &csrf,
        &goal_naming(&request_id(8), agent, legacy, "regression"),
    );
    assert_ne!(
        changed(&head, &body),
        legacy,
        "the browser only guards its current goal's active predicate"
    );
    assert_eq!(
        runner.ran(),
        ["regression"],
        "the browser change starts no baseline"
    );
    runner.release();
    let judged = await_entry(
        &fixture,
        "the old verdict",
        |_, record| matches!(record, Record::PredicateResult { goal, turn: Some(ended), .. } if *goal == old_goal && *ended == turn),
    );
    await_entry(
        &fixture,
        "the decision after the old verdict",
        |lsn, record| {
            lsn > judged
                && matches!(record, Record::LoopDecided { agent: who, .. } if *who == agent)
        },
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// One browser run at a time per agent and per Floor session, and 32 per session. A local goal
/// set is not held behind a browser run, and the browser change it overtakes is refused.
///
/// Mutation: dropping the per-session count lets the 33rd run.
#[test]
fn browser_baselines_are_limited_per_agent_and_session() {
    let fixture = Fixture::new();
    let settings = checks_settings(&fixture);
    let runner = BaselineRunner::new();
    runner.answer("default", common::passes());
    runner.answer("second", common::passes());
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let elsewhere = tempfile::tempdir().expect("a directory outside the workspace");
    let agent = common::hire(&running.handle, elsewhere.path());
    running.handle.ask(Ask::Pause(agent)).expect("local pause");
    let goal = set_goal_once_idle(&running.handle, agent, "first");
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (other_cookie, other_csrf) = exchange(addr, &issue(&settings).bootstrap);

    runner.hold_next();
    let held_change = {
        let (cookie, csrf) = (cookie.clone(), csrf.clone());
        let body = goal_body(&request_id(1), agent, goal, NEW_TEXT);
        std::thread::spawn(move || post_goal(addr, &cookie, &csrf, &body))
    };
    runner.wait_runs(2);
    let (head, body) = post_goal(
        addr,
        &other_cookie,
        &other_csrf,
        &goal_body(&request_id(2), agent, goal, NEW_TEXT),
    );
    assert_eq!(
        refused_reason(&head, &body),
        "check_running",
        "the same agent"
    );
    let (head, body) = post_onboard(addr, &cookie, &csrf, &hire_naming(&attempt(0), "default"));
    assert_eq!(
        refused_reason(&head, &body),
        "check_running",
        "the same session"
    );
    let error = running
        .handle
        .set_goal(
            GoalSetParams {
                may_already_pass: false,
                ..common::goal_params(agent, &[true_program(), "second"])
            },
            &dudes_rpc::HangUp::never(),
        )
        .expect_err("its own check passes");
    assert!(error.message.contains("already passes"), "{error:?}");
    let replaced = common::set_goal(
        &running.handle,
        common::goal_params(agent, &[true_program(), "third"]),
    );
    assert_ne!(replaced, goal);
    runner.release();
    let (head, body) = held_change.join().expect("the held change answers");
    assert_eq!(refused_reason(&head, &body), "stale_goal");

    for n in 1..=32 {
        let (head, body) = post_onboard(
            addr,
            &other_cookie,
            &other_csrf,
            &hire_naming(&attempt(n), "default"),
        );
        assert_eq!(
            refused_reason(&head, &body),
            "check_already_passes",
            "run {n}"
        );
    }
    let (head, body) = post_onboard(
        addr,
        &other_cookie,
        &other_csrf,
        &hire_naming(&attempt(33), "default"),
    );
    assert_eq!(refused_reason(&head, &body), "check_limit");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The snapshot's `checks` sibling lists each enrolled check by name, with no command, timeout or
/// directory, only while Hire or goal change is on. An enrollment whose list would pass its cap
/// is refused when the operator file loads.
///
/// Mutation: dropping the size check loads the oversized enrollment.
#[test]
fn checks_sibling_is_bounded_and_safe() {
    let fixture = Fixture::new();
    let settings = checks_settings(&fixture);
    let daemon = boot(&settings, fake_seams(&settings)).expect("boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let (frame, bytes) = snapshot(addr, &cookie, &csrf);
    let listed: Vec<Value> = CHECKS
        .iter()
        .map(|&(id, label, may_already_pass)| {
            json!({
                "id": id, "label": label, "revision": enrolled_revision(id),
                "may_already_pass": may_already_pass
            })
        })
        .collect();
    assert_eq!(
        frame["checks"],
        json!({"workspaces": [{"workspace_id": "repo_1", "checks": listed}]})
    );
    assert!(
        frame["onboarding"]["workspaces"][0]
            .get("completion_check_label")
            .is_none()
    );
    let text = String::from_utf8(bytes).unwrap();
    assert!(!text.contains("timeout"), "no timeout crosses the view");
    assert!(
        !text.contains(settings.enrollment.profiles[0].cwd.to_str().unwrap()),
        "no directory crosses the view"
    );
    assert!(
        !text.contains(&serde_json::to_string(true_program()).unwrap()),
        "no command crosses the view"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let quiet = Fixture::new();
    let mut off = enrolled_settings(&quiet);
    off.floor_hire_enabled = false;
    let daemon = boot(&off, fake_seams(&off)).expect("boot");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&off).bootstrap);
    let (frame, _) = snapshot(addr, &cookie, &csrf);
    assert!(frame.get("checks").is_none(), "no Hire and no goal change");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);

    let program = serde_json::to_string(true_program()).expect("predicate TOML string");
    let root = tempfile::tempdir().expect("operator config root");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("repository marker");
    let mut source = format!(
        "[budget]\nfloor_usd_per_day = 100\n[budget.per_turn_floor]\nusd = 0.000001\n[[floor.onboarding.workspaces]]\nworkspace_id = 'repo'\nlabel = 'Repository'\ncompletion_check_label = 'Checks pass'\ncwd = '{}'\ndepartment = 'engineering'\nisolation = 'default'\nagent_usd_per_day = 10\ngoal_usd = 5\nhop_cap = 3\npredicate_argv = [{program}]\npredicate_timeout_s = 10\n",
        repo.display()
    );
    for n in 0..100 {
        let _ = write!(
            source,
            "[[floor.onboarding.workspaces.checks]]\nid = 'check-{n}'\nlabel = '{}'\nargv = [{program}]\ntimeout_s = 10\n",
            "x".repeat(100)
        );
    }
    let config = root.path().join("config.toml");
    std::fs::write(&config, &source).expect("over-size check list");
    let Err(error) = load_operator(&config) else {
        panic!("a list over its cap loaded");
    };
    assert!(error.to_string().contains("16384 byte cap"), "{error}");
    let fitting: String = source
        .split("[[floor.onboarding.workspaces.checks]]")
        .take(11)
        .collect::<Vec<_>>()
        .join("[[floor.onboarding.workspaces.checks]]");
    std::fs::write(&config, fitting).expect("ten checks");
    assert_eq!(
        load_operator(&config)
            .expect("ten checks fit")
            .enrollment
            .profiles[0]
            .checks
            .len(),
        11
    );
}

/// SHA-256 over `domain`, then each field as a length-prefixed name and value: how every request
/// digest is framed, recomputed here rather than taken from the daemon.
fn framed(domain: &[u8], fields: &[(&str, &str)]) -> Hex32 {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for (field, value) in fields {
        hasher.update(u16::try_from(field.len()).unwrap().to_be_bytes());
        hasher.update(field.as_bytes());
        hasher.update(u32::try_from(value.len()).unwrap().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    Hex32::new(hasher.finalize().into())
}

/// A local goal of one turn whose check, keyed `key` for [`BaselineRunner`], ran first and failed,
/// spent: the agent is `Exhausted` with nothing left, ready to renew.
fn exhausted_goal(
    running: &common::Running,
    fixture: &Fixture,
    agent: AgentId,
    key: &str,
    may_already_pass: bool,
) -> GoalId {
    running.handle.ask(Ask::Pause(agent)).expect("local pause");
    let goal = running
        .handle
        .set_goal(
            GoalSetParams {
                hop_cap: 1,
                budget: UNCAPPED,
                may_already_pass,
                ..common::goal_params(agent, &[true_program(), key])
            },
            &dudes_rpc::HangUp::never(),
        )
        .expect("the goal's check failed first")
        .goal;
    running
        .handle
        .ask(Ask::Resume(agent))
        .expect("local resume");
    await_entry(
        fixture,
        "the goal's turn",
        |_, record| matches!(record, Record::TurnIssued { agent: who, .. } if *who == agent),
    );
    end_turn(&running.handle, newest_turn(fixture, agent), None);
    await_entry(fixture, "the goal exhausted", |_, record| {
        matches!(
            record,
            Record::LoopDecided { agent: who, decision, .. }
                if *who == agent && *decision == exhausted(dudes_proto::ExhaustedReason::Hops)
        )
    });
    goal
}

/// A renewal that keeps a check that passes is refused with no `GoalSet`. The same renewal naming
/// an enrolled check that fails applies with that check's name under `dudes.renew.v2`, and a kept
/// check the operator says may already pass renews under `dudes.renew.v1` with no run.
///
/// Mutation: writing `dudes.renew.v2` for a kept check makes the kept renewal's digest differ.
#[test]
fn browser_renew_keeps_or_names_a_check() {
    let fixture = Fixture::new();
    let repo = reload_repo(&fixture, "renew-repo");
    let program = serde_json::to_string(true_program()).expect("predicate TOML string");
    let entries = format!(
        "{}[[floor.onboarding.workspaces.checks]]\nid = 'contributing'\nlabel = 'CONTRIBUTING.md exists'\nargv = [{program}, 'contributing']\ntimeout_s = 10\n",
        renew_entry("repo_1", &repo, 3, Some("1"), None)
    );
    let settings = renew_settings(&fixture, &renew_config(&entries));
    let runner = BaselineRunner::new();
    let fixture_parts = seams(
        &settings,
        SharedHost::new(),
        Arc::clone(&runner),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    let daemon = boot(&settings, fixture_parts).expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, &repo);
    let g1 = exhausted_goal(&running, &fixture, agent, "g1", false);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);

    runner.answer("g1", common::passes());
    let (head, response) = post_renew(
        addr,
        &cookie,
        &csrf,
        &renew_body(&request_id(1), agent, g1, SHOWN),
    );
    assert_eq!(refused_reason(&head, &response), "check_already_passes");
    assert!(renewals(&fixture, agent).is_empty(), "nothing renewed");

    let revision = common::check_revision_as(
        "contributing",
        "CONTRIBUTING.md exists",
        &[true_program(), "contributing"],
        10,
        false,
    );
    let mut named = renew_body(&request_id(2), agent, g1, SHOWN);
    named["check_id"] = json!("contributing");
    named["check_revision"] = json!(revision);
    let (head, response) = post_renew(addr, &cookie, &csrf, &named);
    renewed(&head, &response);
    let [(_, goal, attribution)] =
        <[_; 1]>::try_from(renewals(&fixture, agent)).expect("one renewal");
    assert!(attribution.check_chosen);
    assert_eq!(
        goal.completion_check.map(|check| check.id),
        Some("contributing".to_owned())
    );
    assert!(matches!(
        goal.baseline,
        Some(dudes_proto::CheckBaseline::CommandFailed { exit: 1, .. })
    ));
    assert_eq!(
        attribution.request_digest,
        framed(
            b"dudes.renew.v2\0",
            &[
                ("agent", &agent.to_string()),
                ("goal", &g1.to_string()),
                ("text", RENEWED_TEXT),
                ("hop_cap", "3"),
                ("goal_usd_micros", "1000000"),
                ("goal_tokens", ""),
                ("check_id", "contributing"),
                ("check_revision", &revision),
            ]
        ),
        "the digest binds the check"
    );

    let other = common::hire(&running.handle, &repo);
    let kept = exhausted_goal(&running, &fixture, other, "kept", true);
    let ran = runner.ran().len();
    let (head, response) = post_renew(
        addr,
        &cookie,
        &csrf,
        &renew_body(&request_id(3), other, kept, SHOWN),
    );
    renewed(&head, &response);
    assert_eq!(
        runner.ran().len(),
        ran,
        "a kept check that may already pass is not run"
    );
    let [(_, goal, attribution)] =
        <[_; 1]>::try_from(renewals(&fixture, other)).expect("one renewal");
    assert!(!attribution.check_chosen);
    assert_eq!(
        goal.baseline,
        Some(dudes_proto::CheckBaseline::MayAlreadyPass)
    );
    assert_eq!(
        attribution.request_digest,
        Hex32::new(expected_renew_digest(other, kept, RENEWED_TEXT, SHOWN))
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn browser_onboarded_hires_allow_mcp_read_and_noop_but_refuse_changed_sets() {
    let fixture = Fixture::new();
    let settings = enrolled_settings(&fixture);
    let (mut running, addr, cookie, csrf) = every_engine_floor(&settings);
    let (head, _) = post_onboard(addr, &cookie, &csrf, &hire_request('A', Some("codex")));
    assert!(head.starts_with("HTTP/1.1 200 OK"));
    let agent = fixture
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::AgentHired {
                id,
                browser_onboarding: Some(_),
                ..
            } => Some(id),
            _ => None,
        })
        .expect("the recorded browser origin");
    await_continuation_outcome(&fixture, &running.handle, agent);
    let before = fixture.records();
    for names in [None, Some(Vec::new())] {
        let Answered::Mcp(result) = running
            .handle
            .ask(Ask::Mcp(dudes_proto::rpc::agent::McpParams {
                agent,
                mcp_servers: names,
            }))
            .unwrap()
        else {
            panic!("an MCP result");
        };
        assert_eq!(result.agent, agent);
        assert!(result.mcp_servers.is_empty());
        assert!(!result.changed);
    }
    let refused = running
        .handle
        .ask(Ask::Mcp(dudes_proto::rpc::agent::McpParams {
            agent,
            mcp_servers: Some(vec![dudes_types::McpServerName::parse("memory").unwrap()]),
        }))
        .unwrap_err();
    assert_eq!(refused.code, dudes_proto::rpc::ErrorCode::Refused.code());
    assert_eq!(
        refused.message,
        "agent.mcp: changed server sets are refused for browser-onboarded hires"
    );
    assert_eq!(fixture.records(), before);
    running.stop(ExitCause::ShutdownRpc);
}

/// A switch changed in the operator file and adopted by a reload routes at once on the running
/// gateway, with no restart, and an off switch is exactly an unrouted path again.
#[test]
fn a_reload_runs_a_floor_switch_without_a_restart() {
    let fixture = Fixture::new();
    let hire = "[floor.hire]\nenabled = true\n";
    let mut settings = reload_settings(&fixture, hire);
    settings.floor_answer_enabled = false;
    settings.floor_hire_enabled = true;
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
    .expect("boot Floor");
    let addr = daemon.floor_addr().expect("Floor address");
    let mut running = common::start(daemon);
    let (cookie, csrf) = exchange(addr, &issue(&settings).bootstrap);
    let body = serde_json::to_vec(&answer_body(
        &request_id(1),
        AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("an agent id"),
        Lsn(1),
        "main",
    ))
    .expect("answer JSON");
    let answer_route = || {
        let (answered, _) = post_bytes(addr, "/floor/answer", &cookie, &csrf, &body, None);
        let (unrouted, _) = post_bytes(addr, "/floor/unrouted", &cookie, &csrf, &body, None);
        (answered, unrouted)
    };
    let (answered, unrouted) = answer_route();
    assert_eq!(answered, unrouted, "off is an unrouted path");

    let sha256 = rewrite(
        &settings,
        &format!("{hire}[floor.answer]\nenabled = true\n"),
    );
    let result = operator_reload(&settings, apply(&sha256, &[], &[], &[])).expect("reload");
    assert_eq!(result["applied"], json!(true), "{result}");
    assert_eq!(result["switches"], json!(["floor.answer"]), "{result}");
    let (answered, unrouted) = answer_route();
    assert_ne!(answered, unrouted, "the route exists without a restart");
    let (frame, _) = snapshot(addr, &cookie, &csrf);
    assert_eq!(frame["answer"], json!({ "max_text_bytes": 4096 }));

    let sha256 = rewrite(&settings, hire);
    let result = operator_reload(&settings, apply(&sha256, &[], &[], &[])).expect("reload");
    assert_eq!(result["switches"], json!(["floor.answer"]), "{result}");
    let (answered, unrouted) = answer_route();
    assert_eq!(answered, unrouted, "off again is an unrouted path");
    let (frame, _) = snapshot(addr, &cookie, &csrf);
    assert!(frame.get("answer").is_none(), "{frame}");
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}
