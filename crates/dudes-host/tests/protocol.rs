#![expect(
    missing_docs,
    reason = "Integration tests do not expose a library API."
)]

use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use dudes_host::{ChildExit, ChildStream, ChildStreams, ProtocolWriter, RealHost};
use dudes_providers::adapter::{ManagedInstall, TurnSession};
use dudes_providers::codex::app_server::AppServerReader;
use dudes_providers::codex::startup::CodexStartup;
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_providers::protocol::ProtocolAction;
use dudes_providers::stream::TurnReader as _;
use dudes_supervisor::host::{
    AdoptionKey, Liveness, OwnedTargetState, OwnedTargetsStatus, ProcessHost, SpawnResult,
};

mod common;

const THREAD: &str = "00000000-0000-4000-8000-000000000001";

fn wait_file(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "fixture did not release {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn app_server_child() {
    let Some(directory) = std::env::var_os("DUDES_PROTOCOL_CHILD_DIR") else {
        return;
    };
    let directory = PathBuf::from(directory);
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut line = String::new();
    input.read_line(&mut line).unwrap();
    assert!(line.contains(r#""method":"initialize""#));
    writeln!(
        output,
        r#"{{"id":1,"result":{{"userAgent":"dudes/0.160.0 (test)","codexHome":{directory:?}}}}}"#
    )
    .unwrap();
    output.flush().unwrap();
    line.clear();
    input.read_line(&mut line).unwrap();
    assert!(line.contains(r#""method":"initialized""#));
    line.clear();
    input.read_line(&mut line).unwrap();
    assert!(line.contains(r#""method":"thread/start""#));
    let temporary = directory.join("tmp");
    let outbox = directory.join("outbox");
    let roots = if cfg!(any(target_os = "macos", target_os = "linux")) {
        format!("{temporary:?},{outbox:?}")
    } else {
        format!("{temporary:?}")
    };
    writeln!(output, r#"{{"id":2,"result":{{"thread":{{"id":"{THREAD}","status":{{"type":"idle"}}}},"cwd":{directory:?},"approvalPolicy":"never","sandbox":{{"type":"workspaceWrite","writableRoots":[{roots}],"networkAccess":false,"excludeTmpdirEnvVar":true,"excludeSlashTmp":true}}}}}}"#).unwrap();
    output.flush().unwrap();
    output.write_all("{}\n".repeat(400_000).as_bytes()).unwrap();
    output.flush().unwrap();
    line.clear();
    input.read_line(&mut line).unwrap();
    assert!(line.contains(r#""method":"turn/start""#));
    assert!(line.contains(&"x".repeat(1_048_576)));
    std::fs::write(directory.join("task-received"), b"ready").unwrap();
    writeln!(
        output,
        r#"{{"id":3,"result":{{"turn":{{"id":"turn-one","status":"inProgress"}}}}}}"#
    )
    .unwrap();
    writeln!(output, r#"{{"method":"item/started","params":{{"threadId":"{THREAD}","turnId":"turn-one","item":{{"type":"agentMessage","id":"reply-one"}}}}}}"#).unwrap();
    writeln!(output, r#"{{"method":"item/agentMessage/delta","params":{{"threadId":"{THREAD}","turnId":"turn-one","itemId":"reply-one","delta":"hello"}}}}"#).unwrap();
    output.flush().unwrap();
    wait_file(&directory.join("finish"));
    writeln!(output, r#"{{"method":"item/agentMessage/delta","params":{{"threadId":"{THREAD}","turnId":"turn-one","itemId":"reply-one","delta":" world"}}}}"#).unwrap();
    writeln!(output, r#"{{"method":"item/completed","params":{{"threadId":"{THREAD}","turnId":"turn-one","item":{{"type":"agentMessage","id":"reply-one","text":"hello world"}}}}}}"#).unwrap();
    writeln!(output, r#"{{"method":"turn/completed","params":{{"threadId":"{THREAD}","turn":{{"id":"turn-one","status":"completed"}}}}}}"#).unwrap();
    output.flush().unwrap();
    if std::env::var_os("DUDES_PROTOCOL_FAST_EXIT").is_none() {
        line.clear();
        assert_eq!(
            input.read_line(&mut line).unwrap(),
            0,
            "another task reached the child"
        );
        std::fs::write(directory.join("stdin-closed"), b"ready").unwrap();
    }
}

#[test]
fn protocol_child() {
    let Some(directory) = std::env::var_os("DUDES_PROTOCOL_CHILD_DIR") else {
        return;
    };
    let directory = PathBuf::from(directory);
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).unwrap();
    assert!(line.contains(r#""method":"initialize""#));
    std::fs::write(directory.join("initialized"), b"ready").unwrap();
    std::io::stdout()
        .lock()
        .write_all(b"initialized\n")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !directory.join("exit").exists() {
        assert!(
            Instant::now() < deadline,
            "parent never released protocol fixture"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

enum Event {
    Line(String),
    Text(String),
    Failed(AdoptionKey),
    Ended(ChildExit),
}

struct Consumer(mpsc::Sender<Event>);
impl ChildStream for Consumer {
    fn line(&mut self, text: &str) {
        self.0.send(Event::Line(text.to_owned())).unwrap();
    }
    fn ended(self: Box<Self>, exit: ChildExit) {
        self.0.send(Event::Ended(exit)).unwrap();
    }
}

struct Streams(mpsc::Sender<Event>);
impl ChildStreams for Streams {
    fn watch(&self, _key: Option<&AdoptionKey>) -> Box<dyn ChildStream> {
        panic!("protocol child used legacy watcher");
    }
    fn watch_protocol(
        &self,
        _key: &AdoptionKey,
        writer: ProtocolWriter,
    ) -> Option<Box<dyn ChildStream>> {
        writer
            .send(ProtocolAction::Write(CodexStartup::initialize()))
            .unwrap();
        Some(Box::new(Consumer(self.0.clone())))
    }
    fn protocol_failed(&self, key: &AdoptionKey) {
        self.0.send(Event::Failed(key.clone())).unwrap();
    }
}

#[test]
fn protocol_spawn_returns_before_completion_and_early_exit_is_failure() {
    let directory = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::channel();
    let mut host = RealHost::watching(Arc::new(Streams(tx)));
    let spec = LaunchSpec {
        argv: vec![
            std::env::current_exe().unwrap().into_os_string(),
            "--exact".into(),
            "protocol_child".into(),
            "--nocapture".into(),
        ],
        env: vec![EnvVar::new("DUDES_PROTOCOL_CHILD_DIR", directory.path())],
        cwd: directory.path().to_path_buf(),
        stdin: LaunchInput::Protocol(CodexStartup::new(
            directory.path().to_path_buf(),
            directory.path().to_path_buf(),
            directory.path().to_path_buf(),
            "not yet submitted".into(),
            TurnSession::Assigned,
            dudes_providers::adapter::Adapter::model_selection(
                &dudes_providers::codex::managed::CodexAdapter::default(),
            ),
        )),
    };
    let before = Instant::now();
    let SpawnResult::Spawned(key) = host.spawn(&spec, &ManagedInstall::default()) else {
        panic!("protocol child did not spawn");
    };
    assert!(before.elapsed() < Duration::from_secs(5));
    loop {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Event::Line(line) if line == "initialized" => break,
            Event::Line(_) => {}
            Event::Text(_) => panic!("simple fixture emitted parsed text"),
            _ => panic!("protocol ended before child initialization"),
        }
    }
    assert!(directory.path().join("initialized").exists());
    assert_eq!(host.alive(&key), Liveness::Alive);
    let wrong = AdoptionKey {
        pid: key.pid,
        start_time: key.start_time + 1,
        boot: key.boot.clone(),
    };
    host.kill(&wrong);
    assert_eq!(host.alive(&key), Liveness::Alive);
    assert!(
        !rx.try_iter().any(|event| matches!(event, Event::Failed(_))),
        "wrong full key must not cancel this writer"
    );
    std::fs::write(directory.path().join("exit"), b"go").unwrap();
    let mut failed = false;
    loop {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Event::Failed(found) => {
                assert_eq!(
                    (found.pid, found.start_time, found.boot.as_ref()),
                    (key.pid, key.start_time, key.boot.as_ref())
                );
                failed = true;
            }
            Event::Ended(exit) => {
                assert_eq!(exit.code, Some(0));
                assert!(failed, "exit zero cannot certify protocol completion");
                break;
            }
            Event::Line(_) => {}
            Event::Text(_) => panic!("simple fixture emitted parsed text"),
        }
    }
    common::shutdown_retaining_incomplete(&mut host, "protocol child was already reaped");
}

struct AppStreams {
    tx: mpsc::Sender<Event>,
    startup: CodexStartup,
    hold_terminal: bool,
}

struct AppConsumer {
    tx: mpsc::Sender<Event>,
    reader: AppServerReader,
    writer: ProtocolWriter,
    held_directory: Option<PathBuf>,
}

impl ChildStreams for AppStreams {
    fn watch(&self, _key: Option<&AdoptionKey>) -> Box<dyn ChildStream> {
        panic!("app server used legacy watcher");
    }
    fn watch_protocol(
        &self,
        _key: &AdoptionKey,
        writer: ProtocolWriter,
    ) -> Option<Box<dyn ChildStream>> {
        let mut reader = AppServerReader::new(self.startup.clone());
        for action in reader.start() {
            writer.send(action).unwrap();
        }
        Some(Box::new(AppConsumer {
            tx: self.tx.clone(),
            reader,
            writer,
            held_directory: self.hold_terminal.then(|| self.startup.cwd().to_path_buf()),
        }))
    }
    fn protocol_failed(&self, key: &AdoptionKey) {
        self.tx.send(Event::Failed(key.clone())).unwrap();
    }
}

impl ChildStream for AppConsumer {
    fn line(&mut self, text: &str) {
        if text.contains("turn/completed")
            && let Some(directory) = &self.held_directory
        {
            std::fs::write(directory.join("terminal-buffered"), b"ready").unwrap();
            wait_file(&directory.join("consume-terminal"));
        }
        let read = self.reader.line(text);
        for action in read.actions {
            self.writer.send(action).unwrap();
        }
        for block in read.text {
            self.tx.send(Event::Text(block.text)).unwrap();
        }
    }
    fn ended(self: Box<Self>, exit: ChildExit) {
        self.tx.send(Event::Ended(exit)).unwrap();
    }
}

#[test]
fn one_large_task_streams_through_stdout_flood_and_closes_stdin() {
    app_server_turn(false);
}

#[test]
fn buffered_terminal_event_is_parsed_after_reap_before_final_delivery() {
    app_server_turn(true);
}

fn app_server_turn(fast_exit: bool) {
    let directory = tempfile::tempdir().unwrap();
    let startup = CodexStartup::new(
        directory.path().to_path_buf(),
        directory.path().to_path_buf(),
        directory.path().to_path_buf(),
        "x".repeat(1_048_576),
        TurnSession::Assigned,
        dudes_providers::adapter::Adapter::model_selection(
            &dudes_providers::codex::managed::CodexAdapter::default(),
        ),
    );
    let (tx, rx) = mpsc::channel();
    let mut host = RealHost::watching(Arc::new(AppStreams {
        tx,
        startup: startup.clone(),
        hold_terminal: fast_exit,
    }));
    let mut spec = LaunchSpec {
        argv: vec![
            std::env::current_exe().unwrap().into_os_string(),
            "--exact".into(),
            "app_server_child".into(),
            "--nocapture".into(),
        ],
        env: vec![EnvVar::new("DUDES_PROTOCOL_CHILD_DIR", directory.path())],
        cwd: directory.path().to_path_buf(),
        stdin: LaunchInput::Protocol(startup),
    };
    if fast_exit {
        spec.env.push(EnvVar::new("DUDES_PROTOCOL_FAST_EXIT", "1"));
    }
    let key = common::spawned(
        host.spawn(&spec, &ManagedInstall::default()),
        "one protocol process",
    );
    match rx.recv_timeout(Duration::from_secs(15)).unwrap() {
        Event::Text(text) => assert_eq!(text, "hello"),
        _ => panic!("first live fragment missing"),
    }
    assert!(directory.path().join("task-received").exists());
    assert_eq!(host.alive(&key), Liveness::Alive);
    std::fs::write(directory.path().join("finish"), b"go").unwrap();
    if fast_exit {
        wait_file(&directory.path().join("terminal-buffered"));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let evidence = host.owned_targets(1);
            assert_eq!(evidence.status, OwnedTargetsStatus::Complete);
            let [target] = evidence.targets.as_slice() else {
                panic!("expected the one retained protocol child: {evidence:?}");
            };
            let observed = target.key.as_ref().expect("spawned child has a key");
            assert_eq!(
                (observed.pid, observed.start_time, observed.boot.as_ref()),
                (key.pid, key.start_time, key.boot.as_ref())
            );
            if target.state == OwnedTargetState::NativeBoundaryAbsent {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "child was not reaped before final parsing: {:?}",
                target.state
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::write(directory.path().join("consume-terminal"), b"go").unwrap();
    }
    match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        Event::Text(text) => assert_eq!(text, " world"),
        _ => panic!("second live fragment missing"),
    }
    match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        Event::Ended(exit) => assert_eq!(exit.code, Some(0)),
        _ => panic!("valid protocol completion failed"),
    }
    if !fast_exit {
        assert!(directory.path().join("stdin-closed").exists());
    }
    common::shutdown_retaining_incomplete(&mut host, "completed protocol child");
}
