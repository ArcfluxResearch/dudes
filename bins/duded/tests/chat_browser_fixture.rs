//! Disposable real-daemon browser fixture with provider completion controlled by files.
#![cfg(unix)]

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dudes_proto::rpc::{Method, cred::OperatorCredential, floor::SessionIssueResult};
use dudes_providers::claude::{ClaudeAdapter, config::ClaudeConfig};
use dudes_providers::launch::EnvVar;
use dudes_providers::pi::{PiAdapter, config::PiConfig};
use dudes_rpc::Client;
use dudes_supervisor::host::{ProcessHost as _, SpawnResult};
use dudes_supervisor::{Clock, Rng};
use dudes_types::{Provider, Timestamp};
use serde_json::json;

mod common;
#[path = "common/native_chat_host.rs"]
mod native_chat_host;

struct FixtureClock;
impl Clock for FixtureClock {
    fn now(&self) -> Timestamp {
        let elapsed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        Timestamp::from_unix_millis(i64::try_from(elapsed.as_millis()).unwrap())
    }
}

struct FixtureRng;
impl Rng for FixtureRng {
    fn ulid(&mut self) -> ulid::Ulid {
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        ulid::Ulid::from_parts(
            u64::try_from(now.as_millis()).unwrap(),
            u128::from_le_bytes(random),
        )
    }
    fn agent_token(&mut self) -> dudes_proto::rpc::hook::HookToken {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).unwrap();
        let mut encoded = String::with_capacity(43);
        for chunk in bytes.chunks(3) {
            let mut buffer = [0; 3];
            buffer[..chunk.len()].copy_from_slice(chunk);
            let packed =
                (u32::from(buffer[0]) << 16) | (u32::from(buffer[1]) << 8) | u32::from(buffer[2]);
            for index in 0..=chunk.len() {
                encoded.push(char::from(
                    ALPHABET[((packed >> (18 - 6 * index)) & 63) as usize],
                ));
            }
        }
        dudes_proto::rpc::hook::HookToken::new(encoded).unwrap()
    }
}

fn private_json(path: &Path, value: &serde_json::Value) {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(&serde_json::to_vec(value).unwrap()).unwrap();
    file.sync_all().unwrap();
}

fn refresh_launch(settings: &duded::Settings, addr: std::net::SocketAddr, launch: &Path) {
    let credential = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .unwrap();
    let mut client = Client::connect(settings.socket()).unwrap();
    let response = client
        .call_as_operator(
            Method::FloorSessionIssue,
            None,
            OperatorCredential::new(credential).unwrap(),
        )
        .unwrap();
    let issue: SessionIssueResult = serde_json::from_value(response.result.unwrap()).unwrap();
    let mut file = tempfile::NamedTempFile::new_in(&settings.state_dir).unwrap();
    file.write_all(
        &serde_json::to_vec(
            &json!({"url":format!("http://{addr}/floor/#bootstrap={}", issue.bootstrap)}),
        )
        .unwrap(),
    )
    .unwrap();
    file.as_file().sync_all().unwrap();
    file.persist(launch).unwrap();
}

fn fixture_adapter(
    provider: Provider,
    command: PathBuf,
) -> Box<dyn dudes_providers::adapter::Adapter> {
    match provider {
        Provider::Claude => Box::new(ClaudeAdapter::new(ClaudeConfig {
            command: command.into_os_string(),
            ..ClaudeConfig::default()
        })),
        Provider::Pi => Box::new(PiAdapter::new(PiConfig {
            command: command.to_str().unwrap().to_owned(),
        })),
        _ => unreachable!(),
    }
}

#[test]
#[ignore = "interactive browser fixture; requires asset and control-file paths"]
fn interactive_conversation_fixture() {
    let provider_name =
        std::env::var("DUDES_CHAT_FIXTURE_PROVIDER").unwrap_or_else(|_| "claude".into());
    let provider = match provider_name.as_str() {
        "claude" => Provider::Claude,
        "pi" => Provider::Pi,
        _ => panic!("fixture provider must be claude or pi"),
    };
    let assets = PathBuf::from(
        std::env::var_os("DUDES_CHAT_FIXTURE_ASSETS").expect("fixture asset directory"),
    );
    let control = PathBuf::from(
        std::env::var_os("DUDES_CHAT_FIXTURE_CONTROL").expect("new owner-readable control file"),
    );
    let fixture = common::Fixture::new();
    let work = fixture.state.path().join("provider");
    std::fs::create_dir(&work).unwrap();
    let mut settings = fixture.settings();
    settings.floor = Some(duded::settings::FloorSettings {
        assets_dir: assets,
        port: Some(0),
    });
    settings.floor_answer_enabled = true;
    settings.daemon_env = ["PATH", "TMPDIR"]
        .into_iter()
        .filter_map(|name| std::env::var_os(name).map(|value| EnvVar::new(name, value)))
        .collect();
    settings
        .daemon_env
        .push(EnvVar::new("DUDES_CHAT_FIXTURE_WORK", work.as_os_str()));
    settings
        .daemon_env
        .push(EnvVar::new("DUDES_CHAT_FIXTURE_PROVIDER", &provider_name));
    if std::env::var_os("DUDES_CHAT_FIXTURE_HOLD_PREFIX").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        settings
            .daemon_env
            .push(EnvVar::new("DUDES_CHAT_FIXTURE_HOLD_PREFIX", "1"));
    }
    let command = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/conversation-provider.py")
        .canonicalize()
        .unwrap();
    let children = duded::ChildEnds::new();
    let seams = duded::settings::Seams {
        host: native_chat_host::owned_chat_host(Arc::clone(&children)),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(common::ScriptedRunner::scripted([])),
        clock: Arc::new(FixtureClock),
        rng: FixtureRng,
        resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
        adapters: [fixture_adapter(provider, command)].into_iter().collect(),
        children,
        witness: duded::startup::Witness::default(),
    };
    let daemon =
        duded::boot_with_options(&settings, seams, duded::settings::RunOptions::default()).unwrap();
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let mut hire = common::hire_params(fixture.state.path());
    hire.provider = provider;
    if provider == Provider::Pi {
        hire.budget.tokens_per_day = Some(dudes_types::Tokens::new(100_000));
    }
    let duded::ports::Answered::Hired(hired) = running
        .handle
        .ask(duded::ports::Ask::Hire(Box::new(hire)))
        .unwrap()
    else {
        panic!("fixture agent was not hired");
    };
    let agent = hired.agent;
    let launch = fixture.state.path().join("launch.json");
    refresh_launch(&settings, addr, &launch);
    let refresh = fixture.state.path().join("refresh-launch");
    let stop = fixture.state.path().join("stop");
    private_json(
        &control,
        &json!({"origin":format!("http://{addr}"),"provider":provider_name,"agent":agent,"state_dir":fixture.state.path(),"runtime_dir":settings.runtime_dir,"socket":settings.socket(),"work":work,"launch_file":launch,"refresh_launch":refresh,"stop":stop}),
    );
    let deadline = Instant::now() + Duration::from_mins(20);
    while !stop.exists() && Instant::now() < deadline {
        if refresh.exists() {
            refresh_launch(&settings, addr, &launch);
            std::fs::remove_file(&refresh).unwrap();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    running.stop(duded::ExitCause::Signal);
    let _ = fixture.state.keep();
    let _ = fixture.runtime.keep();
}

fn provider_fixture(work: &Path, arguments: &[&str], task_stdin: &str) -> (bool, String) {
    provider_fixture_for(work, None, arguments, task_stdin)
}

fn provider_fixture_for(
    work: &Path,
    provider: Option<&str>,
    arguments: &[&str],
    task_stdin: &str,
) -> (bool, String) {
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/conversation-provider.py");
    let mut argv = vec![script.into_os_string()];
    argv.extend(arguments.iter().map(std::ffi::OsString::from));
    let mut spec = dudes_providers::launch::LaunchSpec {
        argv,
        cwd: work.to_path_buf(),
        stdin: dudes_providers::launch::LaunchInput::WriteThenClose(task_stdin.as_bytes().to_vec()),
        env: vec![
            EnvVar::new("PATH", std::env::var_os("PATH").unwrap()),
            EnvVar::new("DUDES_CHAT_FIXTURE_WORK", work.as_os_str()),
        ],
    };
    if let Some(provider) = provider {
        spec.env
            .push(EnvVar::new("DUDES_CHAT_FIXTURE_PROVIDER", provider));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let mut host = dudes_host::RealHost::watching(Arc::new(FixtureOutput(tx)));
    assert!(matches!(
        host.spawn(&spec, &dudes_providers::adapter::ManagedInstall::default()),
        SpawnResult::Spawned(_)
    ));
    let result = rx.recv_timeout(Duration::from_secs(10));
    let shutdown = host.shutdown();
    if shutdown.is_err() {
        assert!(
            host.owned() > 0,
            "unconfirmed cleanup must retain ownership"
        );
    }
    let (exit, output, overflowed) = result.expect("provider fixture must finish within its bound");
    assert!(!overflowed, "fixture output exceeded its capture bound");
    (exit.code == Some(0), output)
}

struct FixtureOutput(std::sync::mpsc::Sender<(dudes_host::ChildExit, String, bool)>);
struct FixtureLines {
    tx: std::sync::mpsc::Sender<(dudes_host::ChildExit, String, bool)>,
    text: String,
    overflowed: bool,
}
impl dudes_host::ChildStreams for FixtureOutput {
    fn watch(
        &self,
        _key: Option<&dudes_supervisor::host::AdoptionKey>,
    ) -> Box<dyn dudes_host::ChildStream> {
        Box::new(FixtureLines {
            tx: self.0.clone(),
            text: String::new(),
            overflowed: false,
        })
    }
}
impl dudes_host::ChildStream for FixtureLines {
    fn line(&mut self, text: &str) {
        if self.text.len() + text.len() + 1 > 1_048_576 {
            self.overflowed = true;
            return;
        }
        self.text.push_str(text);
        self.text.push('\n');
    }
    fn ended(self: Box<Self>, exit: dudes_host::ChildExit) {
        self.tx.send((exit, self.text, self.overflowed)).unwrap();
    }
}

#[test]
fn pi_fixture_refuses_missing_resume_ambiguous_tasks_and_foreign_selectors() {
    let work = tempfile::tempdir().unwrap();
    let sessions = work.path().join("pi-sessions");
    std::fs::create_dir(&sessions).unwrap();
    let base = [
        "--mode",
        "json",
        "--session-dir",
        sessions.to_str().unwrap(),
    ];
    for suffix in [
        vec!["--session", "missing-session", "--", "Recall the word"],
        vec!["--session-id", "foreign", "--", "Remember amber"],
        vec!["--session", "../../other", "--", "Recall the word"],
        vec!["--session", "session.jsonl", "--", "Recall the word"],
        vec!["--", "first", "second"],
        vec![],
    ] {
        let args: Vec<_> = base.iter().copied().chain(suffix).collect();
        assert!(!provider_fixture_for(work.path(), Some("pi"), &args, "").0);
    }
    let args: Vec<_> = base
        .iter()
        .copied()
        .chain(["--", "Remember amber"])
        .collect();
    assert!(!provider_fixture_for(work.path(), Some("pi"), &args, "Remember violet").0);
    assert!(!work.path().join("launched.1.json").exists());
}

#[test]
fn provider_fixture_refuses_dropped_or_ambiguous_task_delivery() {
    let work = tempfile::tempdir().unwrap();
    for run in [1, 2] {
        std::fs::write(work.path().join(format!("release.{run}")), b"").unwrap();
    }
    let empty = provider_fixture(work.path(), &["--session-id", "empty-task"], "");
    let ambiguous = provider_fixture(
        work.path(),
        &[
            "--session-id",
            "ambiguous-task",
            "--",
            "Remember cobalt_7391",
        ],
        "Remember saffron_8291",
    );
    assert!(!empty.0, "a dropped task must fail");
    assert!(!ambiguous.0, "two task transports must fail");
    assert!(!work.path().join("launched.1.json").exists());
}

#[test]
fn provider_fixture_refuses_resume_without_retained_session() {
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("release.1"), b"").unwrap();
    let output = provider_fixture(
        work.path(),
        &[
            "--resume",
            "missing-session",
            "--",
            "What word did I give you?",
        ],
        "",
    );
    assert!(!output.0, "resume cannot invent past context");
    assert!(!work.path().join("launched.1.json").exists());
}
