//! Signing in to an engine from the Floor, driven through the real gateway by a scripted browser
//! against a stand-in engine whose login prints a link, reads one line and exits.

#![cfg(unix)]

mod common;
use common::PageHttp as _;

use std::collections::BTreeMap;
use std::io::{self, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use common::{Fixture, SharedHost, TestClock};
use duded::floor_browser::Page;
use duded::ports::{OpenedPty, PtyChild, PtyCloseOutcome, PtyHostPort, PtyOpenFailure};
use duded::pty_output::OutputSubscription;
use duded::settings::{EngineLogin, FloorSettings};
use duded::startup::Witness;
use dudes_proto::rpc::pty::PtyId;
use dudes_providers::adapter::Adapter;
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_rpc::methods::pty::decode_terminal_input;
use dudes_supervisor::Clock as _;
use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use serde_json::{Value, json};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v7"
);
const WAIT: Duration = Duration::from_secs(10);
const PROTOCOL: &str = "dudes-engine-login-v1";
const LINK: &str = "https://login.example.invalid/authorize";
const CODE: &str = "the-pasted-code";

/// The shipped one-shot runner, for the status command the Floor's engine rows run.
struct RealRunner(dudes_host::OneShot);

impl duded::predicate::PredicateRunner for RealRunner {
    fn run(
        &self,
        _spec: &LaunchSpec,
        _timeout: Duration,
    ) -> std::io::Result<duded::predicate::PredicateOutcome> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    fn run_discarding_output(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
    ) -> std::io::Result<duded::predicate::PredicateOutcome> {
        Ok(match self.0.run_discarding_output(spec, timeout)? {
            dudes_host::OneShotOutcome::Finished {
                exit,
                output_tail,
                timed_out,
            } => duded::predicate::PredicateOutcome::Finished {
                exit,
                output_tail,
                timed_out,
            },
            dudes_host::OneShotOutcome::Swept => duded::predicate::PredicateOutcome::Swept,
        })
    }

    fn sweep(&self) {
        self.0.sweep();
    }
}

/// A stand-in `claude` that is signed out until its login has read the right code. `login` is
/// the shell the stand-in runs for `auth login`.
fn install_engine(path: &Path, signed: &Path, login: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\n\
             case \"$1 $2\" in\n\
             'auth status') echo run >> '{signed}.runs'; [ -f '{signed}' ]; exit $? ;;\n\
             'auth login') {login} ;;\n\
             esac\n\
             exit 2\n",
            signed = signed.display()
        ),
    )
    .expect("the program");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("executable");
}

/// The login that prints a link, reads the pasted code and signs in when it is the right one.
fn pasted_code(signed: &Path) -> String {
    format!(
        "echo '{LINK}'; printf 'Paste code here if prompted > '; read code; \
         [ \"$code\" = '{CODE}' ] && echo ok > '{signed}' && exit 0; exit 1",
        signed = signed.display()
    )
}

/// The login that starts a background process in its group, names it, and waits.
const WITH_CHILD: &str = "sleep 600 & echo \"child=$!\"; wait";

/// A real terminal host behind the daemon's port, as `main` wires one.
struct RealPty {
    host: dudes_pty::PtyHost,
    drains: BTreeMap<PtyId, std::thread::JoinHandle<()>>,
    outputs: BTreeMap<PtyId, duded::pty_output::PtyOutput>,
}

impl RealPty {
    fn make() -> Box<dyn PtyHostPort> {
        Box::new(Self {
            host: dudes_pty::PtyHost::new(),
            drains: BTreeMap::new(),
            outputs: BTreeMap::new(),
        })
    }

    fn forget(&mut self, pty: PtyId) {
        self.outputs.remove(&pty);
        if let Some(drain) = self.drains.remove(&pty) {
            drain.join().expect("the drain ends with the PTY");
        }
    }
}

impl PtyHostPort for RealPty {
    fn open(
        &mut self,
        spec: dudes_providers::launch::LaunchSpec,
        cols: u16,
        rows: u16,
    ) -> Result<OpenedPty, PtyOpenFailure> {
        let opened =
            dudes_host::serialize_spawn(|| self.host.open(spec, dudes_pty::PtySize { cols, rows }))
                .map_err(|_| PtyOpenFailure::Failed(io::Error::other("the PTY did not open")))?;
        let pid = self.host.pid(opened.id).expect("the child's pid");
        let (sink, output) = duded::pty_output::channel();
        let mut reader = opened.reader;
        let (ready, started) = mpsc::sync_channel(1);
        let drain = std::thread::spawn(move || {
            let mut first_line = Vec::new();
            let mut ready = Some(ready);
            while let Ok(dudes_pty::PtyRead::Data(bytes)) = reader.read() {
                sink.push(&bytes);
                if ready.is_some() {
                    first_line.extend_from_slice(&bytes);
                    if first_line.contains(&b'\n') {
                        let _ = ready.take().expect("the first line").send(());
                    }
                }
            }
        });
        self.drains.insert(opened.id, drain);
        self.outputs.insert(opened.id, output);
        started.recv_timeout(WAIT).expect("the login is ready");
        Ok(OpenedPty {
            pty: opened.id,
            child: PtyChild { pid },
        })
    }

    fn write(&mut self, pty: PtyId, bytes: &[u8]) -> io::Result<usize> {
        self.host.write(pty, bytes)
    }

    fn write_ready(&mut self, pty: PtyId, bytes: &[u8]) -> io::Result<usize> {
        self.host.write_ready(pty, bytes)
    }

    fn resize(&mut self, pty: PtyId, cols: u16, rows: u16) -> io::Result<()> {
        self.host.resize(pty, dudes_pty::PtySize { cols, rows })
    }

    fn subscribe(&mut self, pty: PtyId) -> Option<OutputSubscription> {
        self.outputs.get(&pty)?.subscribe()
    }

    fn close(&mut self, pty: PtyId) -> PtyCloseOutcome {
        match self.host.close(pty) {
            Ok(_) => {
                self.forget(pty);
                PtyCloseOutcome::Closed
            }
            Err(error) => PtyCloseOutcome::Failed(error),
        }
    }

    fn exit_success(&mut self, pty: PtyId) -> io::Result<Option<bool>> {
        Ok(self.host.exit_status(pty)?.map(|status| status.success()))
    }

    fn shutdown(&mut self) -> io::Result<()> {
        let result = self.host.shutdown();
        for pty in self.drains.keys().copied().collect::<Vec<_>>() {
            self.forget(pty);
        }
        result
    }
}

struct Floor {
    running: common::Running,
    clock: Arc<TestClock>,
    settings: duded::Settings,
    addr: SocketAddr,
}

fn floor(fixture: &Fixture, bin: &Path, timeout: Duration) -> Floor {
    let mut settings = fixture.settings();
    // The stand-in finds the system's `sleep` on the same path it was found on.
    let mut path = bin.as_os_str().to_owned();
    path.push(":/usr/bin:/bin");
    settings.daemon_env = vec![
        EnvVar::new("PATH", path),
        EnvVar::new("HOME", fixture.state.path()),
        EnvVar::new("DUDES_TEST_OTHER_KEY", "not-for-the-engine"),
    ];
    settings.floor_hire_enabled = true;
    settings.engine_login = Some(EngineLogin {
        host: RealPty::make,
        timeout,
    });
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    let clock = Arc::new(TestClock::new());
    let seams = duded::Seams {
        adapters: dudes_providers::registry::Adapters::new([Box::new(ClaudeAdapter::new(
            ClaudeConfig {
                command: "prog".into(),
                ..ClaudeConfig::default()
            },
        )) as Box<dyn Adapter>]),
        ..common::seams(
            &settings,
            SharedHost::new(),
            Arc::new(RealRunner(dudes_host::OneShot::new())),
            clock.clone(),
            Witness::default(),
        )
    };
    let daemon = duded::boot(&settings, seams).expect("boot");
    let addr = daemon.floor_addr().expect("the Floor listener");
    Floor {
        running: common::start(daemon),
        clock,
        settings,
        addr,
    }
}

fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("response head");
        head.push(byte[0]);
    }
    String::from_utf8(head).expect("ASCII head")
}

/// A signed-in page: a browser with a key of its own, signing with the daemon's fake clock.
fn session(floor: &Floor) -> Page {
    let clock = Arc::clone(&floor.clock);
    Page::enrol_new_on(
        floor.addr,
        &common::floor_link(&floor.settings),
        move || clock.now().unix_millis(),
    )
}

struct Socket {
    stream: TcpStream,
    /// Everything the terminal has shown so far.
    shown: String,
}

impl Socket {
    fn open(page: &Page, path: &str, protocol: &str) -> Self {
        let mut stream = TcpStream::connect(page.addr).expect("Floor listener");
        stream.set_read_timeout(Some(WAIT)).expect("timeout");
        stream
            .write_all(page.upgrade_head(path, protocol).as_bytes())
            .expect("upgrade");
        let head = read_head(&mut stream);
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        Self {
            stream,
            shown: String::new(),
        }
    }

    fn send(&mut self, value: &Value) {
        let payload = value.to_string().into_bytes();
        let mask = [0x5a_u8, 0xa5, 0x3c, 0xc3];
        let mut frame = vec![0x81];
        match payload.len() {
            len @ 0..=125 => frame.push(0x80 | u8::try_from(len).expect("short")),
            len => {
                frame.push(0x80 | 0x7e);
                frame.extend(u16::try_from(len).expect("medium").to_be_bytes());
            }
        }
        frame.extend(mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(i, byte)| byte ^ mask[i % 4]),
        );
        self.stream.write_all(&frame).expect("a frame");
    }

    /// The next text message, or `None` on a close frame.
    fn next(&mut self) -> Option<Value> {
        let mut head = [0_u8; 2];
        self.stream.read_exact(&mut head).expect("a frame head");
        let short = head[1] & 0x7f;
        let length = if short <= 125 {
            usize::from(short)
        } else {
            let mut bytes = [0; 2];
            self.stream.read_exact(&mut bytes).expect("u16 length");
            usize::from(u16::from_be_bytes(bytes))
        };
        let mut payload = vec![0; length];
        self.stream.read_exact(&mut payload).expect("frame payload");
        match head[0] {
            0x81 => Some(serde_json::from_slice(&payload).expect("JSON message")),
            0x88 => None,
            other => panic!("unexpected opcode {other:#x}"),
        }
    }

    /// Messages up to the first of `kind`, taking terminal output on the way.
    fn until(&mut self, kind: &str) -> Value {
        loop {
            let message = self
                .next()
                .unwrap_or_else(|| panic!("closed before {kind}"));
            if message["type"] == kind {
                return message;
            }
            if message["type"] == "login.started" {
                continue;
            }
            assert_eq!(message["type"], "login.data", "{message}");
            let (bytes, _) =
                decode_terminal_input(message["data"].as_str().expect("data")).expect("base64");
            self.shown.push_str(&String::from_utf8_lossy(&bytes));
        }
    }

    /// Read until the terminal has shown `needle`.
    fn show(&mut self, needle: &str) {
        let mut tries = 0;
        while !self.shown.contains(needle) {
            tries += 1;
            assert!(tries < 50, "never showed {needle:?}: {:?}", self.shown);
            let message = self.next().expect("the terminal is open");
            assert_eq!(message["type"], "login.data", "{message}");
            let (bytes, _) =
                decode_terminal_input(message["data"].as_str().expect("data")).expect("base64");
            self.shown.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
}

/// What the Floor's snapshot says about the one engine. A status command that has not answered
/// yet reads `unknown`, so a slow machine is asked again.
fn engine_sign_in(page: &Page) -> Value {
    let mut word = Value::Null;
    for _ in 0..4 {
        let mut stream = Socket::open(page, "/floor/stream", "dudes-floor-v4");
        stream.send(&json!({"type": "floor.subscribe"}));
        let snapshot = stream.until("floor.snapshot");
        word = snapshot["daemon"]["providers"][0]["sign_in"].clone();
        if word != "unknown" {
            break;
        }
    }
    word
}

/// Start `claude`'s sign-in on a new socket and wait for the terminal to show `shown`.
fn start(page: &Page, shown: &str) -> Socket {
    let mut login = Socket::open(page, "/floor/engine-login", PROTOCOL);
    login.send(&json!({"type": "login.start", "engine": "claude", "cols": 80, "rows": 24}));
    login.until("login.started");
    login.show(shown);
    login
}

fn child_of(login: &Socket) -> i32 {
    let digits: String = login
        .shown
        .split("child=")
        .nth(1)
        .expect("the child's pid")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().expect("a pid")
}

fn gone(pid: i32) -> bool {
    matches!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH))
}

fn await_gone(pid: i32) {
    let give_up = Instant::now() + WAIT;
    while !gone(pid) {
        assert!(
            Instant::now() < give_up,
            "process {pid} outlived its sign-in"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn stop(floor: &mut Floor) {
    assert_eq!(
        floor.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

#[test]
fn the_page_signs_in_to_an_engine_and_the_engine_reads_as_signed_in() {
    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    let signed = fixture.state.path().join("signed-in");
    install_engine(&bin.join("prog"), &signed, &pasted_code(&signed));
    let mut floor = floor(&fixture, &bin, Duration::from_secs(60));
    let page = session(&floor);
    assert_eq!(engine_sign_in(&page), "signed_out");

    let mut login = start(&page, "Paste code here");
    assert!(login.shown.contains(LINK), "{:?}", login.shown);
    login.send(&json!({
        "type": "login.write",
        "data": common::base64_standard(format!("{CODE}\n").as_bytes()),
    }));
    let ended = login.until("login.ended");
    assert_eq!(ended["outcome"], "succeeded");
    assert!(login.next().is_none(), "the socket closes after the end");

    assert_eq!(engine_sign_in(&page), "signed_in");

    let journal: Vec<u8> = std::fs::read_dir(fixture.journal_dir())
        .expect("journal directory")
        .flatten()
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .flatten()
        .collect();
    for secret in [LINK, CODE] {
        assert!(
            !journal
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "the journal holds {secret}"
        );
    }
    stop(&mut floor);
}

#[test]
fn closing_the_page_ends_the_logins_whole_process_group() {
    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    install_engine(
        &bin.join("prog"),
        &fixture.state.path().join("s"),
        WITH_CHILD,
    );
    let mut floor = floor(&fixture, &bin, Duration::from_secs(60));
    let page = session(&floor);
    let login = start(&page, "child=");
    let child = child_of(&login);
    assert!(
        !gone(child),
        "the login's child runs while the page is open"
    );
    drop(login);
    await_gone(child);
    stop(&mut floor);
}

#[test]
fn a_second_sign_in_for_the_engine_ends_the_first() {
    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    install_engine(
        &bin.join("prog"),
        &fixture.state.path().join("s"),
        WITH_CHILD,
    );
    let mut floor = floor(&fixture, &bin, Duration::from_secs(60));
    let (first_page, second_page) = (session(&floor), session(&floor));
    let mut first = start(&first_page, "child=");
    let child = child_of(&first);
    let second = start(&second_page, "child=");
    assert_eq!(first.until("login.ended")["outcome"], "replaced");
    await_gone(child);
    let kept = child_of(&second);
    assert!(!gone(kept), "the newer sign-in keeps running");
    drop(second);
    await_gone(kept);
    stop(&mut floor);
}

#[test]
fn the_time_limit_ends_the_login_and_the_daemon_typed_nothing() {
    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    install_engine(
        &bin.join("prog"),
        &fixture.state.path().join("s"),
        WITH_CHILD,
    );
    let mut floor = floor(&fixture, &bin, Duration::from_millis(1_500));
    let page = session(&floor);
    let mut login = start(&page, "child=");
    let child = child_of(&login);
    assert_eq!(login.until("login.ended")["outcome"], "timed_out");
    await_gone(child);
    assert_eq!(login.shown, format!("child={child}\r\n"));
    stop(&mut floor);
}

#[test]
fn stopping_the_daemon_ends_the_login() {
    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    install_engine(
        &bin.join("prog"),
        &fixture.state.path().join("s"),
        WITH_CHILD,
    );
    let mut floor = floor(&fixture, &bin, Duration::from_secs(60));
    let page = session(&floor);
    let login = start(&page, "child=");
    let child = child_of(&login);
    stop(&mut floor);
    await_gone(child);
    drop(login);
}

/// Send `messages` pastes of 4 KiB, the most one message carries, to a login that reads none of it.
fn paste(login: &mut Socket, messages: usize) {
    let block = common::base64_standard(&[b'a'; 4096]);
    for _ in 0..messages {
        login.send(&json!({"type": "login.write", "data": block}));
    }
}

/// Bound on how long a close, a time limit or a stop may take behind a flood of typing.
const END_BOUND: Duration = Duration::from_secs(3);

fn ended_within_bound(pid: i32) {
    let began = Instant::now();
    await_gone(pid);
    assert!(
        began.elapsed() < END_BOUND,
        "the end waited behind the typing"
    );
}

#[test]
fn a_flood_of_typing_cannot_hold_back_closing_the_page() {
    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    install_engine(
        &bin.join("prog"),
        &fixture.state.path().join("s"),
        WITH_CHILD,
    );
    let mut floor = floor(&fixture, &bin, Duration::from_secs(60));
    let page = session(&floor);
    let mut login = start(&page, "child=");
    let child = child_of(&login);
    paste(&mut login, 6);
    drop(login);
    ended_within_bound(child);
    stop(&mut floor);
}

#[test]
fn a_flood_of_typing_cannot_hold_back_the_time_limit() {
    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    install_engine(
        &bin.join("prog"),
        &fixture.state.path().join("s"),
        WITH_CHILD,
    );
    let mut floor = floor(&fixture, &bin, Duration::from_millis(1_500));
    let page = session(&floor);
    let mut login = start(&page, "child=");
    let child = child_of(&login);
    paste(&mut login, 6);
    assert_eq!(login.until("login.ended")["outcome"], "timed_out");
    ended_within_bound(child);
    stop(&mut floor);
}

#[test]
fn a_flood_of_typing_cannot_hold_back_stopping_the_daemon() {
    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    install_engine(
        &bin.join("prog"),
        &fixture.state.path().join("s"),
        WITH_CHILD,
    );
    let mut floor = floor(&fixture, &bin, Duration::from_secs(60));
    let page = session(&floor);
    let mut login = start(&page, "child=");
    let child = child_of(&login);
    paste(&mut login, 6);
    stop(&mut floor);
    ended_within_bound(child);
    drop(login);
}

#[test]
fn typing_over_the_limits_ends_the_sign_in() {
    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    install_engine(
        &bin.join("prog"),
        &fixture.state.path().join("s"),
        WITH_CHILD,
    );
    let mut floor = floor(&fixture, &bin, Duration::from_secs(60));
    let page = session(&floor);
    let mut login = start(&page, "child=");
    let child = child_of(&login);
    login.send(&json!({
        "type": "login.write",
        "data": common::base64_standard(&[b'a'; 4097]),
    }));
    loop {
        if login.next().is_none() {
            break;
        }
    }
    ended_within_bound(child);
    stop(&mut floor);
}

#[test]
fn a_sign_in_starts_in_an_empty_directory_with_a_minimal_environment() {
    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    install_engine(
        &bin.join("prog"),
        &fixture.state.path().join("s"),
        "echo \"cwd=$(pwd -P) other=${DUDES_TEST_OTHER_KEY-unset} home=${HOME-unset} files=$(ls | wc -l)\"",
    );
    let mut floor = floor(&fixture, &bin, Duration::from_secs(60));
    let page = session(&floor);
    let mut login = Socket::open(&page, "/floor/engine-login", PROTOCOL);
    login.send(&json!({"type": "login.start", "engine": "claude", "cols": 80, "rows": 24}));
    login.until("login.ended");
    let line = login
        .shown
        .lines()
        .find(|line| line.starts_with("cwd="))
        .expect("the report");
    assert!(line.contains(" other=unset "), "{line}");
    assert!(
        line.contains(&format!(" home={}", fixture.state.path().display())),
        "{line}"
    );
    assert!(
        line.trim_end().ends_with('0') && line.contains("files="),
        "{line}"
    );
    let cwd = line
        .split_whitespace()
        .next()
        .expect("cwd")
        .trim_start_matches("cwd=");
    assert!(
        !cwd.starts_with(
            &*fixture
                .state
                .path()
                .canonicalize()
                .expect("state")
                .to_string_lossy()
        ),
        "{cwd}"
    );
    let give_up = Instant::now() + WAIT;
    while Path::new(cwd).exists() {
        assert!(
            Instant::now() < give_up,
            "the scratch directory outlived the sign-in"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    stop(&mut floor);
}

#[test]
fn signing_in_to_pi_needs_the_terminal_switch() {
    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    install_engine(
        &bin.join("prog"),
        &fixture.state.path().join("s"),
        WITH_CHILD,
    );
    let mut floor = floor(&fixture, &bin, Duration::from_secs(60));
    let page = session(&floor);
    let mut login = Socket::open(&page, "/floor/engine-login", PROTOCOL);
    login.send(&json!({"type": "login.start", "engine": "pi", "cols": 80, "rows": 24}));
    assert_eq!(login.until("login.refused")["reason"], "needs_terminal");
    assert!(login.next().is_none());
    stop(&mut floor);
}

#[test]
fn the_floor_asks_the_status_command_once_in_the_reuse_window() {
    let fixture = Fixture::new();
    let bin = fixture.state.path().join("bin");
    let signed = fixture.state.path().join("signed-in");
    install_engine(&bin.join("prog"), &signed, WITH_CHILD);
    let mut floor = floor(&fixture, &bin, Duration::from_secs(60));
    let page = session(&floor);
    for _ in 0..3 {
        assert_eq!(engine_sign_in(&page), "signed_out");
    }
    let runs = std::fs::read_to_string(fixture.state.path().join("signed-in.runs")).expect("runs");
    assert_eq!(
        runs.lines().count(),
        1,
        "a signed-out answer is reused like a signed-in one"
    );
    stop(&mut floor);
}
