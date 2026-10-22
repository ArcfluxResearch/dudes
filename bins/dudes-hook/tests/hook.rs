//! The hook shim's process interface over the real platform local transport.

use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dudes_proto::reply::VendorReply;
use dudes_proto::rpc::cred::{OPERATOR_CREDENTIAL_LEN, OperatorCredential};
use dudes_proto::rpc::hook::{HookEventParams, HookToken};
use dudes_proto::rpc::{ErrorCode, Method, Request, Response, RpcError};
use dudes_rpc::{
    Connection as _, Listener as _, LocalClient, LocalTransport, MAX_FRAME_BYTES, read_frame,
    write_frame,
};

const FAILURE: &[u8] = b"dudes-hook: hook delivery failed\n";
const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
enum InputDelivery {
    Required,
    MayCloseEarly,
}

fn input_delivery(result: std::io::Result<()>, expectation: InputDelivery) -> std::io::Result<()> {
    match (result, expectation) {
        (Err(error), InputDelivery::MayCloseEarly)
            if error.kind() == std::io::ErrorKind::BrokenPipe =>
        {
            Ok(())
        }
        (result, _) => result,
    }
}

fn bind(root: &Path) -> LocalTransport {
    let credential = OperatorCredential::new("B".repeat(OPERATOR_CREDENTIAL_LEN)).unwrap();
    LocalTransport::bind_in_with_credential_and_halt(
        root.join("run"),
        "duded.sock",
        &credential,
        dudes_host::disconnect_named_pipe,
    )
    .expect("bind platform local transport")
}
#[expect(
    clippy::disallowed_types,
    reason = "the process test must execute the binary to observe its status"
)]
fn spawn_hook(
    endpoint: Option<&Path>,
    token: Option<&OsStr>,
    input: &[u8],
    delivery: InputDelivery,
) -> Child {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_dudes-hook"));
    command
        .env_remove("DUDES_SOCK")
        .env_remove("DUDES_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(endpoint) = endpoint {
        command.env("DUDES_SOCK", endpoint);
    }
    if let Some(token) = token {
        command.env("DUDES_TOKEN", token);
    }
    let mut child = command.spawn().expect("spawn dudes-hook");
    input_delivery(child.stdin.take().unwrap().write_all(input), delivery)
        .expect("write dudes-hook input");
    child
}
#[cfg(unix)]
fn non_unicode_token() -> OsString {
    use std::os::unix::ffi::OsStringExt as _;
    OsString::from_vec(vec![0xff])
}
#[cfg(windows)]
fn non_unicode_token() -> OsString {
    use std::os::windows::ffi::OsStringExt as _;
    OsString::from_wide(&[0xd800])
}

fn wait_bounded(mut child: Child) -> Output {
    let deadline = Instant::now() + WAIT;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("dudes-hook did not exit within {WAIT:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    child.wait_with_output().expect("collect hook output")
}

#[cfg(windows)]
#[test]
fn the_hook_refuses_unsupported_local_pipe_spellings() {
    for endpoint in [
        "relative.pipe",
        r"\\?\pipe\dudes",
        r"\\.\pipe\dudes/invalid",
    ] {
        let endpoint = Path::new(endpoint);
        let error = LocalClient::connect(endpoint).expect_err("unsupported pipe spelling");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let output = wait_bounded(spawn_hook(
            Some(endpoint),
            Some(OsStr::new(TOKEN)),
            br#"{"hook_event_name":"Stop"}"#,
            InputDelivery::Required,
        ));
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, FAILURE);
    }
}

fn serve_one(
    transport: LocalTransport,
    reply: Result<VendorReply, RpcError>,
) -> JoinHandle<Result<Request, String>> {
    std::thread::spawn(move || {
        let (connection, _) = transport.accept().map_err(|error| error.to_string())?;
        let (mut reader, mut writer) = connection.split().map_err(|error| error.to_string())?;
        let frame = read_frame(&mut reader)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "the accepted peer sent no frame".to_owned())?;
        let request: Request = serde_json::from_str(&frame).map_err(|error| error.to_string())?;
        let response = match reply {
            Ok(reply) => Response::ok(
                request.id.clone(),
                serde_json::to_value(reply).map_err(|error| error.to_string())?,
            ),
            Err(error) => Response::err(request.id.clone(), error),
        };
        write_frame(&mut writer, &response).map_err(|error| error.to_string())?;
        Ok(request)
    })
}

fn wake(endpoint: &Path) {
    if let Ok(mut client) = LocalClient::connect(endpoint) {
        let _ = client.call(Method::SystemPing, None);
    }
}
fn round_trip(reply: Result<VendorReply, RpcError>, raw: &serde_json::Value) -> (Request, Output) {
    let root = tempfile::tempdir().unwrap();
    let transport = bind(root.path());
    let endpoint = transport.path().to_path_buf();
    let server = serve_one(transport, reply);
    let input = serde_json::to_vec(raw).unwrap();
    let output = wait_bounded(spawn_hook(
        Some(&endpoint),
        Some(OsStr::new(TOKEN)),
        &input,
        InputDelivery::Required,
    ));
    wake(&endpoint);
    let request = server.join().unwrap().expect("listener observed a request");
    (request, output)
}

#[test]
fn hook_event_round_trips_over_local_transport() {
    let body = serde_json::json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse", "permissionDecision": "deny"
    }});
    let raw = serde_json::json!({
        "hook_event_name": "PreToolUse", "tool_name": "Bash",
        "tool_input": {"command": "cargo test"}
    });
    let reply = VendorReply {
        body: Some(body.clone()),
        exit_code: 2,
    };
    let (request, output) = round_trip(Ok(reply), &raw);
    assert_eq!(request.method, Method::HookEvent.name());
    assert!(request.cred.is_none());
    let params: HookEventParams = serde_json::from_value(request.params.unwrap()).unwrap();
    assert_eq!(params.token, HookToken::new(TOKEN).unwrap());
    assert_eq!(params.raw, raw);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stdout, serde_json::to_vec(&body).unwrap());
    assert!(output.stderr.is_empty());
    let raw = serde_json::json!({"hook_event_name": "SessionEnd"});
    let (_, output) = round_trip(
        Ok(VendorReply {
            body: None,
            exit_code: 0,
        }),
        &raw,
    );
    assert!(output.stdout.is_empty(), "body: None writes zero bytes");
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn overloaded_pre_tool_and_prompt_block_but_other_events_fail_open() {
    let overloaded = RpcError {
        code: ErrorCode::InternalError.code(),
        message: "hook event was not queued because hook capacity is full".into(),
        data: None,
    };
    for event in ["PreToolUse", "UserPromptSubmit"] {
        let (_, output) = round_trip(
            Err(overloaded.clone()),
            &serde_json::json!({"hook_event_name": event}),
        );
        assert_eq!(output.status.code(), Some(2), "{event}: {output:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(
            output.stderr,
            b"dudes-hook: daemon overloaded; control step blocked; retry, and restart duded if this persists\n"
        );
    }
    for event in ["PostToolUse", "Stop"] {
        let (_, other) = round_trip(
            Err(overloaded.clone()),
            &serde_json::json!({"hook_event_name": event}),
        );
        assert_eq!(other.status.code(), Some(0), "{event}");
        assert!(other.stdout.is_empty());
        assert_eq!(other.stderr, FAILURE);
    }
    let (_, different_code) = round_trip(
        Err(RpcError {
            code: ErrorCode::InvalidParams.code(),
            ..overloaded
        }),
        &serde_json::json!({"hook_event_name": "PreToolUse"}),
    );
    assert_eq!(different_code.status.code(), Some(0));
    assert!(different_code.stdout.is_empty());
    assert_eq!(different_code.stderr, FAILURE);
}
#[test]
fn independently_returned_control_errors_keep_the_existing_fail_open() {
    for event in ["PreToolUse", "UserPromptSubmit"] {
        for error in [
            RpcError {
                code: dudes_proto::rpc::JOURNAL_STOPPED_CODE,
                message: "journal writes stopped".to_owned(),
                data: Some(serde_json::json!({"lsn":9})),
            },
            RpcError {
                code: ErrorCode::Refused.code(),
                message: "authorization refused".to_owned(),
                data: None,
            },
            RpcError {
                code: ErrorCode::InvalidParams.code(),
                message: "translation refused".to_owned(),
                data: None,
            },
        ] {
            let (_, output) = round_trip(Err(error), &serde_json::json!({"hook_event_name":event}));
            assert_eq!(output.status.code(), Some(0), "{event}: {output:?}");
            assert!(output.stdout.is_empty());
            assert_eq!(output.stderr, FAILURE);
        }
    }
}

#[test]
fn a_half_written_answer_keeps_the_control_events_fail_open() {
    for event in ["PreToolUse", "UserPromptSubmit"] {
        let root = tempfile::tempdir().unwrap();
        let transport = bind(root.path());
        let endpoint = transport.path().to_path_buf();
        let server = std::thread::spawn(move || {
            let (connection, _) = transport.accept().unwrap();
            let (mut reader, mut writer) = connection.split().unwrap();
            read_frame(&mut reader).unwrap().expect("a request");
            writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":").unwrap();
            writer.flush().unwrap();
        });
        let input = serde_json::to_vec(&serde_json::json!({"hook_event_name": event})).unwrap();
        let output = wait_bounded(spawn_hook(
            Some(&endpoint),
            Some(OsStr::new(TOKEN)),
            &input,
            InputDelivery::Required,
        ));
        server.join().unwrap();
        assert_eq!(output.status.code(), Some(0), "{event}: {output:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, FAILURE);
    }
}

#[test]
fn a_connection_dropped_after_the_request_blocks_control_events_and_fails_open_for_others() {
    const UNANSWERED_BLOCK: &[u8] = b"dudes-hook: daemon accepted the connection but did not answer; control step blocked; retry, and restart duded if this persists\n";
    for (event, control) in [
        ("PreToolUse", true),
        ("UserPromptSubmit", true),
        ("Stop", false),
        ("PostToolUse", false),
        ("Notification", false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let transport = bind(root.path());
        let endpoint = transport.path().to_path_buf();
        let server = std::thread::spawn(move || {
            let (connection, _) = transport.accept().unwrap();
            let (mut reader, writer) = connection.split().unwrap();
            read_frame(&mut reader).unwrap().expect("a request");
            drop(writer);
            drop(reader);
        });
        let input = serde_json::to_vec(&serde_json::json!({"hook_event_name": event})).unwrap();
        let output = wait_bounded(spawn_hook(
            Some(&endpoint),
            Some(OsStr::new(TOKEN)),
            &input,
            InputDelivery::Required,
        ));
        server.join().unwrap();
        assert!(output.stdout.is_empty(), "{event}: {output:?}");
        if control {
            assert_eq!(output.status.code(), Some(2), "{event}: {output:?}");
            assert_eq!(output.stderr, UNANSWERED_BLOCK, "{event}");
        } else {
            assert_eq!(output.status.code(), Some(0), "{event}: {output:?}");
            assert_eq!(output.stderr, FAILURE, "{event}");
        }
    }
}

#[derive(Clone, Copy)]
enum EndpointInput {
    Bound,
    Missing,
    Empty,
    NotListening,
}

fn invalid_did_not_connect(kind: EndpointInput, token: Option<&OsStr>, input: &[u8]) {
    let root = tempfile::tempdir().unwrap();
    let transport = bind(root.path());
    let endpoint = transport.path().to_path_buf();
    let selected = match kind {
        EndpointInput::Bound => Some(endpoint.clone()),
        EndpointInput::Missing => None,
        EndpointInput::Empty => Some(PathBuf::new()),
        EndpointInput::NotListening => Some(root.path().join("no-listener")),
    };
    let server = serve_one(
        transport,
        Ok(VendorReply {
            body: None,
            exit_code: 0,
        }),
    );
    let output = wait_bounded(spawn_hook(
        selected.as_deref(),
        token,
        input,
        InputDelivery::MayCloseEarly,
    ));
    assert_eq!(output.status.code(), Some(0), "fail open");
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, FAILURE);
    let transcript = [output.stdout, output.stderr].concat();
    for secret in [TOKEN, "candidate-secret-with-bad-length"] {
        assert!(
            !transcript
                .windows(secret.len())
                .any(|part| part == secret.as_bytes())
        );
    }
    wake(&endpoint);
    let request = server.join().unwrap().expect("listener stayed usable");
    assert_eq!(
        request.method,
        Method::SystemPing.name(),
        "invalid hook connected"
    );
}
const FORBIDDEN: &[&str] = &[
    "process::Command",
    "Command::new",
    "tokio::process",
    "xshell",
    "duct::",
    "libc::system",
    "posix_spawn",
    "execve",
    "execvp",
    "\"sh\"",
    "\"bash\"",
    "\"cmd\"",
    "\"powershell\"",
    "\"-c\"",
];

fn forbidden(source: &str) -> Vec<&'static str> {
    FORBIDDEN
        .iter()
        .copied()
        .filter(|needle| source.contains(needle))
        .collect()
}

fn allowed_environment(source: &str) -> bool {
    source.matches("std::env::var_os").count() == 1
        && source.matches("required(\"").count() == 2
        && source.matches("required(\"DUDES_SOCK\")").count() == 1
        && source.matches("required(\"DUDES_TOKEN\")").count() == 1
        && !source.contains("std::env::var(")
        && !source.contains("std::env::vars")
        && !source.contains("std::env::args")
        && !source.contains("DUDES_RUNTIME_DIR")
        && !source.contains("\"HOME\"")
}
#[test]
fn hook_missing_or_invalid_environment_never_connects_or_leaks_token() {
    let valid = br#"{"hook_event_name":"Stop"}"#;
    let malformed = OsStr::new("candidate-secret-with-bad-length");
    let non_unicode = non_unicode_token();
    let oversized = serde_json::to_vec(&"x".repeat(MAX_FRAME_BYTES)).unwrap();
    for (endpoint, token, input) in [
        (
            EndpointInput::Missing,
            Some(OsStr::new(TOKEN)),
            valid.as_slice(),
        ),
        (
            EndpointInput::Empty,
            Some(OsStr::new(TOKEN)),
            valid.as_slice(),
        ),
        (
            EndpointInput::NotListening,
            Some(OsStr::new(TOKEN)),
            valid.as_slice(),
        ),
        (EndpointInput::Bound, None, valid.as_slice()),
        (EndpointInput::Bound, Some(malformed), valid.as_slice()),
        (
            EndpointInput::Bound,
            Some(non_unicode.as_os_str()),
            valid.as_slice(),
        ),
        (EndpointInput::Bound, Some(OsStr::new(TOKEN)), b""),
        (EndpointInput::Bound, Some(OsStr::new(TOKEN)), b"{"),
        (EndpointInput::Bound, Some(OsStr::new(TOKEN)), b"{}\n{}"),
        (
            EndpointInput::Bound,
            Some(OsStr::new(TOKEN)),
            oversized.as_slice(),
        ),
    ] {
        invalid_did_not_connect(endpoint, token, input);
    }
    let source = include_str!("../src/main.rs");
    assert!(
        forbidden(source).is_empty(),
        "forbidden routes: {:?}",
        forbidden(source)
    );
    for needle in FORBIDDEN {
        assert!(
            forbidden(needle).contains(needle),
            "scanner missed {needle}"
        );
    }
    assert!(allowed_environment(source));
    assert!(!allowed_environment(&format!(
        "{source}\nlet _ = required(\"HOME\");"
    )));
}

#[test]
fn invalid_environment_tolerates_only_an_early_closed_stdin() {
    let broken_pipe = || std::io::Error::from(std::io::ErrorKind::BrokenPipe);
    assert!(
        input_delivery(Err(broken_pipe()), InputDelivery::MayCloseEarly).is_ok(),
        "an invalid environment may make the child exit before the fixture writes"
    );
    assert_eq!(
        input_delivery(Err(broken_pipe()), InputDelivery::Required)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::BrokenPipe,
        "valid delivery remains strict"
    );
    assert_eq!(
        input_delivery(
            Err(std::io::Error::from(std::io::ErrorKind::Interrupted)),
            InputDelivery::MayCloseEarly,
        )
        .unwrap_err()
        .kind(),
        std::io::ErrorKind::Interrupted,
        "invalid delivery suppresses no other write failure"
    );
}
