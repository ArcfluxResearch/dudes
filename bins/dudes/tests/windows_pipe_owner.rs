//! A Windows client authenticates the connected pipe before it can present a credential.
#![cfg(windows)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::windows::io::AsHandle as _;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use interprocess::os::windows::named_pipe::{PipeListenerOptions, PipeMode, pipe_mode};

mod common;

use common::{Cli, Fixture, OpenRunner, SharedHost, start};

const HELPER_ROLE: &str = "DUDES_PIPE_OWNER_HELPER_ROLE";
const HELPER_ENDPOINT: &str = "DUDES_PIPE_OWNER_HELPER_ENDPOINT";
const SQUATTER: &str = "pipe-squatter";
const BOUND: &str = "PIPE_OWNER_BOUND";
const ACCEPTED: &str = "PIPE_OWNER_ACCEPTED";
const UNTRUSTED: &str = "PIPE_OWNER_UNTRUSTED_DESCRIPTOR";
const NO_CREDENTIAL: &str = "PIPE_OWNER_NO_CREDENTIAL";
const CREDENTIAL_PRESENT: &str = "PIPE_OWNER_CREDENTIAL_PRESENT";
const WAIT: Duration = Duration::from_secs(10);

#[test]
fn the_cli_refuses_unsupported_local_pipe_spellings() {
    for endpoint in [
        "relative.pipe",
        r"\\?\pipe\dudes",
        r"\\.\pipe\dudes/invalid",
    ] {
        let mut child = Cli::bare()
            .env("DUDES_SOCK", endpoint)
            .args(["--json", "ping"])
            .spawn_captured();
        let deadline = Instant::now() + WAIT;
        while child.try_wait().expect("poll CLI").is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("CLI did not refuse the pipe path within {WAIT:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().expect("collect CLI refusal");
        assert_eq!(
            output.status.code(),
            Some(i32::from(dudes::exit::UNREACHABLE))
        );
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).expect("CLI diagnostic");
        assert!(stderr.contains("must use \\\\.\\pipe\\"), "{stderr}");
    }
}

/// The helper's process boundary is the behavior under test: the hostile listener must not share
/// the CLI process whose credential presentation it observes.
#[expect(
    clippy::disallowed_types,
    reason = "the squatter must be a second process; argv is typed and names no interpreter"
)]
fn helper_command(endpoint: &Path) -> std::process::Command {
    let mut command =
        std::process::Command::new(std::env::current_exe().expect("current test executable"));
    command
        .args([
            "--exact",
            "the_pipe_squatter_helper",
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .env(HELPER_ROLE, SQUATTER)
        .env(HELPER_ENDPOINT, endpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn marker(text: &str) {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{text}").expect("write helper marker");
    stdout.flush().expect("flush helper marker");
}

struct Squatter {
    child: Option<Child>,
    lines: Receiver<String>,
    reader: Option<JoinHandle<()>>,
    seen: Vec<String>,
}

impl Squatter {
    fn spawn(endpoint: &Path) -> Self {
        let mut child = helper_command(endpoint)
            .spawn()
            .expect("spawn pipe squatter");
        let stdout = child.stdout.take().expect("capture squatter stdout");
        let (send, lines) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                let _ = send.send(line);
            }
        });
        Self {
            child: Some(child),
            lines,
            reader: Some(reader),
            seen: Vec::new(),
        }
    }

    fn wait_for(&mut self, marker: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!(
                    "squatter never reported {marker}: {error}; output lines: {:?}",
                    self.seen
                )
            });
            let found = line.contains(marker);
            self.seen.push(line);
            if found {
                return;
            }
        }
    }

    fn finish(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let deadline = Instant::now() + WAIT;
        loop {
            match child.try_wait().expect("poll squatter") {
                Some(status) => {
                    assert!(status.success(), "squatter failed: {:?}", self.seen);
                    break;
                }
                None if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("squatter did not exit within {WAIT:?}: {:?}", self.seen);
                }
            }
        }
        if let Some(reader) = self.reader.take() {
            reader.join().expect("join squatter output reader");
        }
    }
}

impl Drop for Squatter {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn descriptor_is_expected(sid: &str, owner: &str, dacl: &dudes_win::Dacl) -> bool {
    owner == sid
        && dacl
            == &dudes_win::Dacl::Present {
                allowed: vec![sid.to_owned()],
                denied: Vec::new(),
                unread: 0,
            }
}

/// Invoked only by [`an_untrusted_pipe_owner_never_receives_the_operator_credential`].
#[test]
#[ignore = "second-process entry point"]
fn the_pipe_squatter_helper() {
    if std::env::var(HELPER_ROLE).as_deref() != Ok(SQUATTER) {
        return;
    }
    let endpoint = std::env::var_os(HELPER_ENDPOINT).expect("helper endpoint");
    let listener = PipeListenerOptions::new()
        .path(Path::new(&endpoint))
        .mode(PipeMode::Bytes)
        .accept_remote(false)
        .create_duplex::<pipe_mode::Bytes>()
        .expect("bind squatting pipe");
    marker(BOUND);

    let mut stream = listener.accept().expect("accept CLI");
    marker(ACCEPTED);
    let sid = dudes_win::current_user_sid().expect("current TokenUser");
    let owner = dudes_win::read_owner(stream.as_handle()).expect("read squatter owner");
    let dacl = dudes_win::read_dacl(stream.as_handle()).expect("read squatter DACL");
    assert!(
        !descriptor_is_expected(&sid, &owner, &dacl),
        "the hostile fixture accidentally created the trusted descriptor"
    );
    marker(UNTRUSTED);

    let mut request = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        match stream.read(&mut byte).expect("read CLI request") {
            0 => break,
            1 => {
                request.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            _ => unreachable!("one-byte read returned more than one byte"),
        }
    }

    let credential_present = if request.is_empty() {
        false
    } else {
        let request: serde_json::Value =
            serde_json::from_slice(&request).expect("credential-bearing JSON request");
        let id = request
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let response = serde_json::json!({
            "jsonrpc": dudes_proto::rpc::JSONRPC,
            "id": id,
            "result": {}
        });
        serde_json::to_writer(&mut stream, &response).expect("write response");
        stream.write_all(b"\n").expect("terminate response");
        stream.flush().expect("flush response");
        request.get("cred").is_some_and(|value| !value.is_null())
    };
    marker(if credential_present {
        CREDENTIAL_PRESENT
    } else {
        NO_CREDENTIAL
    });
}

/// Fails if the real CLI connects to the first pipe owner and writes the credential.
/// Passes when it connects, rejects the descriptor, and closes without writing a frame.
#[test]
fn an_untrusted_pipe_owner_never_receives_the_operator_credential() {
    let root = tempfile::tempdir().expect("temporary root");
    let runtime = root.path().join("run");
    std::fs::create_dir(&runtime).expect("runtime directory");
    std::fs::write(
        runtime.join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        "A".repeat(dudes_proto::rpc::cred::OPERATOR_CREDENTIAL_LEN),
    )
    .expect("test credential");
    let endpoint = dudes_rpc::local_endpoint(&runtime, "duded.sock");
    let mut squatter = Squatter::spawn(&endpoint);
    squatter.wait_for(BOUND);

    let run = Cli::new(&endpoint, &runtime).args(["--json", "ping"]).run();
    squatter.wait_for(ACCEPTED);
    squatter.wait_for(UNTRUSTED);
    squatter.wait_for(NO_CREDENTIAL);
    assert_eq!(
        run.code(),
        dudes::exit::UNREACHABLE,
        "the CLI accepted an untrusted pipe descriptor\n{}",
        run.output()
    );
    squatter.finish();
}

/// The refusal arm cannot be satisfied by a client that rejects every Windows pipe.
#[test]
fn the_legitimate_windows_daemon_still_completes_an_operator_call() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let mut running = start(
        &settings,
        SharedHost::new(),
        std::sync::Arc::new(OpenRunner::default()),
    );
    let endpoint = dudes_rpc::local_endpoint(&settings.runtime_dir, &settings.socket_name);
    let run = Cli::new(&endpoint, &settings.runtime_dir)
        .arg("--json")
        .args(common::hire_args(fixture.state.path()))
        .run();
    assert_eq!(
        run.code(),
        dudes::exit::SUCCESS,
        "the protected daemon must still admit its operator credential\n{}",
        run.output()
    );
    running.stop();
}
