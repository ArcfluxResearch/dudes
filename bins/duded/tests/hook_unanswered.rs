//! The real shim against a real daemon whose connection cap is full: a control step is refused
//! because the daemon accepted the connection and ended it without an answer.

mod common;

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use common::hook_shim::{generated_handlers, handler, minted_token, payload, spawn_handler};
use common::{
    Fixture, ScriptedRunner, SharedHost, TestClock, goal_params, hire, seams_with, set_goal,
};
use duded::boot;
use duded::ports::Ask;
use duded::startup::Witness;
use dudes_proto::PeerProvenance;
use dudes_proto::rpc::hook::{HookEventParams, HookToken};
use dudes_proto::rpc::{Method, Response};
use dudes_rpc::auth::{PeerIdentity, PeerResolution, PeerResolver};
use dudes_rpc::{ClientError, FrameError, LocalClient};
use dudes_types::AgentId;
use serde_json::{Value, json};

const BLOCK: &[u8] = b"dudes-hook: daemon accepted the connection but did not answer; control step blocked; retry, and restart duded if this persists\n";
const CONTROL: [&str; 2] = ["PreToolUse", "UserPromptSubmit"];
const RUNS: usize = 1000;

#[derive(Debug)]
struct Resolver {
    provenance: Mutex<PeerProvenance>,
    refuse: AtomicBool,
}

impl PeerResolver for Resolver {
    fn resolve(&self, _: &PeerIdentity) -> PeerResolution {
        if self.refuse.load(Ordering::SeqCst) {
            PeerResolution::Refuse
        } else {
            PeerResolution::Resolved(*self.provenance.lock().unwrap())
        }
    }
}

struct Rig {
    fixture: Fixture,
    running: common::Running,
    socket: std::path::PathBuf,
    agent: AgentId,
    token: HookToken,
    resolver: Arc<Resolver>,
}

fn rig() -> Rig {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.max_connections = NonZeroUsize::new(1).unwrap();
    let resolver = Arc::new(Resolver {
        provenance: Mutex::new(PeerProvenance::Unresolvable),
        refuse: AtomicBool::new(false),
    });
    let daemon = boot(
        &settings,
        seams_with(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
            Arc::clone(&resolver) as Arc<dyn PeerResolver>,
        ),
    )
    .expect("boot daemon");
    let socket = daemon.socket().to_path_buf();
    let running = common::start(daemon);
    let agent = hire(&running.handle, fixture.state.path());
    set_goal(
        &running.handle,
        goal_params(agent, &[common::true_program()]),
    );
    let token = minted_token(&fixture, agent);
    *resolver.provenance.lock().unwrap() = PeerProvenance::Agent(agent);
    Rig {
        fixture,
        running,
        socket,
        agent,
        token,
        resolver,
    }
}

fn run_hook(rig: &Rig, socket: &Path, event: &str) -> std::process::Output {
    let settings = generated_handlers(rig.agent, rig.fixture.state.path());
    spawn_handler(
        handler(&settings, event),
        socket,
        &rig.token,
        &payload(event),
    )
    .1
}

fn denied(event: &str, output: &std::process::Output) -> bool {
    output.status.code() == Some(2)
        && serde_json::from_slice::<Value>(&output.stdout).is_ok_and(|body| {
            if event == "PreToolUse" {
                body["hookSpecificOutput"]["permissionDecision"] == json!("deny")
            } else {
                body["decision"] == json!("block")
            }
        })
}

/// Holds the only slot by completing one call and then staying idle.
/// The daemon frees the previous shim's slot only after it sees that shim close, so the holder
/// retries until it is admitted.
fn hold_the_slot(socket: &Path) -> LocalClient {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(mut holder) = LocalClient::connect(socket)
            && holder.call(Method::SystemPing, None).is_ok()
        {
            return holder;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the holder was never admitted"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Retries a shim run until it is answered rather than refused for want of a slot.
fn run_hook_until_answered(rig: &Rig, token: &HookToken, event: &str) -> std::process::Output {
    let settings = generated_handlers(rig.agent, rig.fixture.state.path());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let output = spawn_handler(
            handler(&settings, event),
            &rig.socket,
            token,
            &payload(event),
        )
        .1;
        if output.status.code() == Some(0) || std::time::Instant::now() >= deadline {
            return output;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn a_full_cap_blocks_a_paused_agents_control_steps_and_nothing_else() {
    let rig = rig();
    assert!(matches!(
        rig.running.handle.ask(Ask::Pause(rig.agent)),
        Ok(duded::ports::Answered::Done)
    ));

    // Reference: with a free slot the paused control step is denied. The slot is freed when the
    // daemon sees the previous shim close, so the first run can race it and is retried.
    for event in CONTROL {
        let mut answered = false;
        for _ in 0..200 {
            if denied(event, &run_hook(&rig, &rig.socket, event)) {
                answered = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            answered,
            "{event}: a paused agent is denied with a free slot"
        );
    }

    let holder = hold_the_slot(&rig.socket);
    for event in CONTROL {
        for run in 0..RUNS {
            let output = run_hook(&rig, &rig.socket, event);
            assert_eq!(
                output.status.code(),
                Some(2),
                "{event} run {run}: {output:?}"
            );
            assert!(output.stdout.is_empty(), "{event}: no body on the block");
            assert_eq!(output.stderr, BLOCK, "{event}");
        }
    }
    for event in ["Stop", "SubagentStop", "PostToolUse", "Notification"] {
        let output = run_hook(&rig, &rig.socket, event);
        assert_eq!(output.status.code(), Some(0), "{event}: {output:?}");
        assert!(output.stdout.is_empty(), "{event}");
    }
    drop(holder);
}

fn paused_rig() -> Rig {
    let rig = rig();
    assert!(matches!(
        rig.running.handle.ask(Ask::Pause(rig.agent)),
        Ok(duded::ports::Answered::Done)
    ));
    rig
}

fn assert_fails_open(event: &str, output: &std::process::Output) {
    assert_eq!(output.status.code(), Some(0), "{event}: {output:?}");
    assert!(output.stdout.is_empty(), "{event}: {output:?}");
}

#[test]
fn an_absent_socket_lets_a_paused_agents_control_steps_through() {
    let rig = paused_rig();
    let absent = rig.fixture.state.path().join("no-such.sock");
    for event in CONTROL {
        assert_fails_open(event, &run_hook(&rig, &absent, event));
    }
}

#[cfg(unix)]
#[test]
fn a_socket_file_left_by_a_stopped_daemon_lets_control_steps_through() {
    let rig = paused_rig();
    let stale = rig.fixture.state.path().join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).expect("bind then close"));
    assert!(stale.exists(), "the socket file stays behind");
    for event in CONTROL {
        assert_fails_open(event, &run_hook(&rig, &stale, event));
    }
}

#[test]
fn an_unset_socket_variable_lets_control_steps_through() {
    let rig = paused_rig();
    let settings = generated_handlers(rig.agent, rig.fixture.state.path());
    for event in CONTROL {
        let command = handler(&settings, event).command.clone().expect("command");
        #[allow(
            clippy::disallowed_types,
            reason = "the test runs the shim without DUDES_SOCK"
        )]
        let mut child = std::process::Command::new(command)
            .env_remove("DUDES_SOCK")
            .env("DUDES_TOKEN", rig.token.expose())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn the shim");
        // With no socket to call, the shim may exit before it reads its input.
        match std::io::Write::write_all(
            &mut child.stdin.take().unwrap(),
            &serde_json::to_vec(&payload(event)).unwrap(),
        ) {
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
            result => result.expect("deliver the hook payload"),
        }
        let output = common::hook_shim::wait_bounded(child);
        assert_fails_open(event, &output);
    }
}

#[test]
fn an_error_answer_keeps_the_control_steps_fail_open() {
    let rig = paused_rig();
    let unknown = HookToken::new("B".repeat(43)).unwrap();
    for event in CONTROL {
        let output = run_hook_until_answered(&rig, &unknown, event);
        assert_fails_open(event, &output);
        assert!(!output.stderr.starts_with(b"dudes-hook: daemon accepted"));
    }
}

#[test]
fn a_running_agent_with_a_free_slot_is_allowed() {
    let rig = rig();
    for event in CONTROL {
        let output = run_hook_until_answered(&rig, &rig.token, event);
        assert_eq!(output.status.code(), Some(0), "{event}: {output:?}");
        assert!(!denied(event, &output), "{event}: {output:?}");
        assert!(output.stderr.is_empty(), "{event}: {output:?}");
    }
}

/// Names the shape of an unanswered conversation, and fails on any shape the shim would not block on.
fn unanswered_shape(result: Result<Response, ClientError>) -> String {
    use std::io::ErrorKind::{BrokenPipe, ConnectionReset, NotConnected, UnexpectedEof};

    let (label, error) = match result {
        Err(ClientError::Closed) => return "Closed".to_owned(),
        Err(ClientError::Frame(FrameError::Io(e))) => ("Frame(Io)", e),
        Err(ClientError::Io(e)) => ("Io", e),
        other => panic!("the cap-full call produced {other:?}"),
    };
    assert!(
        matches!(
            error.kind(),
            BrokenPipe | NotConnected | ConnectionReset | UnexpectedEof
        ),
        "unclassified kind {:?}",
        error.kind()
    );
    format!("{label} {:?}", error.kind())
}

fn hook_params(rig: &Rig) -> Value {
    serde_json::to_value(HookEventParams {
        token: HookToken::new(rig.token.expose()).unwrap(),
        raw: payload("PreToolUse"),
    })
    .unwrap()
}

/// Which error a client sees depends on whether its write beats the daemon's close, so one run
/// cannot require any one shape. Every outcome must still be one the shim blocks on, and none may
/// be an answer.
#[test]
fn a_full_cap_never_answers_and_ends_in_a_shape_the_shim_blocks_on() {
    let rig = paused_rig();
    let holder = hold_the_slot(&rig.socket);
    let params = hook_params(&rig);
    for _ in 0..RUNS {
        let mut client = LocalClient::connect(&rig.socket).expect("connect succeeds at the cap");
        unanswered_shape(client.call(Method::HookEvent, Some(params.clone())));
    }
    drop(holder);
}

/// The daemon ends a connection it will not serve before reading from it, so a client that has
/// not written yet reads a clean end of stream.
#[test]
fn a_full_cap_ends_a_silent_connection_with_a_clean_close() {
    let rig = paused_rig();
    let holder = hold_the_slot(&rig.socket);
    let mut client = LocalClient::connect(&rig.socket).expect("connect succeeds at the cap");
    assert!(matches!(client.recv(), Err(ClientError::Closed)));
    drop(holder);
}

/// A client that writes after the daemon ended the connection fails with a broken pipe.
#[test]
fn a_write_after_the_full_cap_close_is_a_broken_pipe() {
    let rig = paused_rig();
    let holder = hold_the_slot(&rig.socket);
    let params = hook_params(&rig);
    let mut client = LocalClient::connect(&rig.socket).expect("connect succeeds at the cap");
    assert!(matches!(client.recv(), Err(ClientError::Closed)));
    let shape = unanswered_shape(client.call(Method::HookEvent, Some(params)));
    assert!(shape.ends_with("BrokenPipe"), "{shape}");
    drop(holder);
}

#[test]
fn a_peer_the_daemon_could_not_place_has_its_control_steps_blocked() {
    let rig = rig();
    rig.resolver.refuse.store(true, Ordering::SeqCst);
    for event in CONTROL {
        let output = run_hook(&rig, &rig.socket, event);
        assert_eq!(output.status.code(), Some(2), "{event}: {output:?}");
        assert_eq!(output.stderr, BLOCK);
    }
    drop(rig.running);
}
