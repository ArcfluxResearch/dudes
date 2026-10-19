//! The real shim against a real daemon whose connection cap is full: a control step is refused
//! because the daemon accepted the connection and ended it without an answer.

mod common;

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

use common::hook_shim::{generated_handlers, handler, minted_token, payload, spawn_handler};
use common::{
    Fixture, ScriptedRunner, SharedHost, TestClock, goal_params, hire, seams_with, set_goal,
};
use duded::boot;
use duded::ports::Ask;
use duded::startup::Witness;
use dudes_journal::Reader;
use dudes_proto::PeerProvenance;
use dudes_proto::rpc::hook::{HookEventParams, HookToken};
use dudes_proto::rpc::{Method, Response};
use dudes_rpc::auth::{PeerIdentity, PeerResolution, PeerResolver};
use dudes_rpc::{ClientError, FrameError, LocalClient};
use dudes_supervisor::host::Clock;
use dudes_types::AgentId;
use dudes_types::{Lsn, Timestamp};
use serde_json::{Value, json};

const BLOCK: &[u8] = b"dudes-hook: daemon accepted the connection but did not answer; control step blocked; retry, and restart duded if this persists\n";
const CONTROL: [&str; 2] = ["PreToolUse", "UserPromptSubmit"];
const RUNS: usize = 1000;
const SHIM_RUNS: usize = 25;

#[derive(Clone, Copy, Eq, PartialEq)]
enum MintPhase {
    Idle,
    Armed,
    Waiting,
    Released,
}

struct MintClock {
    inner: TestClock,
    journal: std::path::PathBuf,
    publication: Mutex<Option<duded::classify::TokenPublicationWork>>,
    phase: Mutex<MintPhase>,
    changed: Condvar,
}

impl MintClock {
    fn new(journal: std::path::PathBuf) -> Self {
        Self {
            inner: TestClock::new(),
            journal,
            publication: Mutex::new(None),
            phase: Mutex::new(MintPhase::Idle),
            changed: Condvar::new(),
        }
    }

    fn wait_reached(&self) {
        let (phase, _) = self
            .changed
            .wait_timeout_while(
                self.phase.lock().unwrap(),
                std::time::Duration::from_secs(10),
                |phase| *phase != MintPhase::Waiting,
            )
            .unwrap();
        let reached = *phase == MintPhase::Waiting;
        drop(phase);
        assert!(reached, "the mint was not held");
    }

    fn release(&self) {
        *self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = MintPhase::Released;
        self.changed.notify_all();
    }
}

impl Clock for MintClock {
    fn now(&self) -> Timestamp {
        let armed = *self.phase.lock().unwrap() == MintPhase::Armed;
        if armed && std::thread::current().name() == Some("duded-under-test") {
            let minted = Reader::new(&self.journal)
                .iter_from(Lsn::ZERO)
                .unwrap()
                .any(|entry| {
                    matches!(
                        entry.unwrap().1.record,
                        dudes_proto::Record::HookTokenMinted { .. }
                    )
                });
            let copied = self
                .publication
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .copied_entries();
            if minted && copied == 0 {
                let mut phase = self.phase.lock().unwrap();
                if *phase != MintPhase::Armed {
                    return self.inner.now();
                }
                *phase = MintPhase::Waiting;
                self.changed.notify_all();
                let (returned, _) = self
                    .changed
                    .wait_timeout_while(phase, std::time::Duration::from_secs(10), |phase| {
                        *phase == MintPhase::Waiting
                    })
                    .unwrap();
                let released = *returned == MintPhase::Released;
                drop(returned);
                assert!(released, "the mint gate was not released");
            }
        }
        self.inner.now()
    }
}

struct ReleaseMint<'a>(&'a MintClock);

impl Drop for ReleaseMint<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

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
    rpc_connections: Box<dyn Fn() -> usize + Send + Sync>,
}

fn rig() -> Rig {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.max_connections = NonZeroUsize::new(1).unwrap();
    let resolver = Arc::new(Resolver {
        provenance: Mutex::new(PeerProvenance::Unresolvable),
        refuse: AtomicBool::new(false),
    });
    let parts = seams_with(
        &settings,
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
        Arc::clone(&resolver) as Arc<dyn PeerResolver>,
    );
    let tokens = parts.rng.tokens();
    let daemon = boot(&settings, parts).expect("boot daemon");
    let rpc_connections = daemon.rpc_connections_for_test();
    let socket = daemon.socket().to_path_buf();
    let running = common::start(daemon);
    let agent = hire(&running.handle, fixture.state.path());
    set_goal(
        &running.handle,
        goal_params(agent, &[common::true_program()]),
    );
    let token = minted_token(&fixture, &tokens, agent);
    await_hook_subject(&fixture, &running, agent);
    *resolver.provenance.lock().unwrap() = PeerProvenance::Agent(agent);
    Rig {
        fixture,
        running,
        socket,
        agent,
        token,
        resolver,
        rpc_connections,
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

fn await_hook_subject(fixture: &Fixture, running: &common::Running, agent: AgentId) {
    let turn = fixture.await_record("the hook subject's issued turn", |record| match record {
        dudes_proto::Record::TurnIssued {
            agent: owner, turn, ..
        } if *owner == agent => Some(*turn),
        _ => None,
    });
    fixture.await_record("the hook subject's actual managed spawn", |record| {
        matches!(record, dudes_proto::Record::TurnSpawned { agent: owner, turn: seen, .. }
            if *owner == agent && *seen == turn)
        .then_some(())
    });
    running
        .handle
        .doctor()
        .expect("finish the hook subject's setup pump");
}

fn await_rpc_connections(count: &dyn Fn() -> usize, expected: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let observed = count();
        if observed == expected {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "expected {expected} RPC connections, observed {observed}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn hold_the_slot(rig: &Rig) -> LocalClient {
    await_rpc_connections(rig.rpc_connections.as_ref(), 0);
    let mut holder = LocalClient::connect(&rig.socket).expect("connect the holder");
    let response = holder
        .call(Method::SystemPing, None)
        .expect("admit the holder");
    assert!(response.error.is_none(), "{response:?}");
    holder
}

fn run_hook_with_a_free_slot(rig: &Rig, token: &HookToken, event: &str) -> std::process::Output {
    await_rpc_connections(rig.rpc_connections.as_ref(), 0);
    let settings = generated_handlers(rig.agent, rig.fixture.state.path());
    spawn_handler(
        handler(&settings, event),
        &rig.socket,
        token,
        &payload(event),
    )
    .1
}

#[test]
fn a_full_cap_blocks_a_paused_agents_control_steps_and_nothing_else() {
    let rig = rig();
    assert!(matches!(
        rig.running.handle.ask(Ask::Pause(rig.agent)),
        Ok(duded::ports::Answered::Done)
    ));

    for event in CONTROL {
        let output = run_hook_with_a_free_slot(&rig, &rig.token, event);
        assert!(denied(event, &output), "{event}: {output:?}");
    }

    let holder = hold_the_slot(&rig);
    for event in CONTROL {
        for run in 0..SHIM_RUNS {
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
        let output = run_hook_with_a_free_slot(&rig, &unknown, event);
        assert_fails_open(event, &output);
        assert!(!output.stderr.starts_with(b"dudes-hook: daemon accepted"));
    }
}

#[test]
fn a_running_agent_with_a_free_slot_is_allowed() {
    let rig = rig();
    for event in CONTROL {
        let output = run_hook_with_a_free_slot(&rig, &rig.token, event);
        assert_eq!(output.status.code(), Some(0), "{event}: {output:?}");
        assert!(!denied(event, &output), "{event}: {output:?}");
        assert!(output.stderr.is_empty(), "{event}: {output:?}");
    }
}

fn unpublished_token_refusal(socket: &Path, token: &HookToken) -> dudes_proto::rpc::RpcError {
    let response = LocalClient::connect(socket)
        .unwrap()
        .call(
            Method::HookEvent,
            Some(
                serde_json::to_value(HookEventParams {
                    token: HookToken::new(token.expose()).unwrap(),
                    raw: payload("PreToolUse"),
                })
                .unwrap(),
            ),
        )
        .expect("the unpublished token refusal is answered");
    response
        .error
        .expect("the durable mint is not yet published")
}

#[test]
fn a_durable_token_mint_alone_does_not_make_the_hook_subject_ready() {
    use std::io::Write as _;

    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.max_connections = NonZeroUsize::MIN;
    settings.floor = Some(duded::settings::FloorSettings {
        assets_dir: std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/floor-release-v3"
        )),
        port: Some(0),
    });
    let clock = Arc::new(MintClock::new(fixture.journal_dir()));
    let resolver = Arc::new(Resolver {
        provenance: Mutex::new(PeerProvenance::Unresolvable),
        refuse: AtomicBool::new(false),
    });
    let parts = seams_with(
        &settings,
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
        Arc::clone(&resolver) as Arc<dyn PeerResolver>,
    );
    let tokens = parts.rng.tokens();
    let daemon = boot(
        &settings,
        duded::Seams {
            host: parts.host,
            pty_host: parts.pty_host,
            predicates: parts.predicates,
            clock: Arc::clone(&clock),
            rng: parts.rng,
            resolver: parts.resolver,
            adapters: parts.adapters,
            children: parts.children,
            witness: parts.witness,
        },
    )
    .unwrap();
    *clock.publication.lock().unwrap() = Some(daemon.token_publication_work());
    let rpc_connections = daemon.rpc_connections_for_test();
    let socket = daemon.socket().to_path_buf();
    let running = common::start(daemon);
    let agent = hire(&running.handle, fixture.state.path());
    let handlers = generated_handlers(agent, fixture.state.path());
    std::thread::scope(|scope| {
        let _release = ReleaseMint(&clock);
        *clock.phase.lock().unwrap() = MintPhase::Armed;
        let setting = scope.spawn(|| {
            set_goal(
                &running.handle,
                goal_params(agent, &[common::true_program()]),
            )
        });
        clock.wait_reached();
        let token = minted_token(&fixture, &tokens, agent);
        *resolver.provenance.lock().unwrap() = PeerProvenance::Agent(agent);
        let output = spawn_handler(
            handler(&handlers, "PreToolUse"),
            &socket,
            &token,
            &payload("PreToolUse"),
        )
        .1;
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert_eq!(
            output.stderr, b"dudes-hook: hook delivery failed\n",
            "{output:?}"
        );
        await_rpc_connections(rpc_connections.as_ref(), 0);
        let refusal = unpublished_token_refusal(&socket, &token);
        assert_eq!(refusal.code, -32004);
        assert_eq!(
            refusal.message,
            "the presented token matches no minting for a live agent"
        );
        assert_eq!(refusal.data, Some(json!({"gate": "token"})));
        writeln!(
            std::io::stderr(),
            "unpublished token reply: {}",
            serde_json::to_string(&refusal).unwrap()
        )
        .unwrap();
        clock.release();
        setting.join().unwrap();
        await_hook_subject(&fixture, &running, agent);
        for event in CONTROL {
            await_rpc_connections(rpc_connections.as_ref(), 0);
            let ready =
                spawn_handler(handler(&handlers, event), &socket, &token, &payload(event)).1;
            assert_eq!(ready.status.code(), Some(0), "{event}: {ready:?}");
            assert!(!denied(event, &ready), "{event}: {ready:?}");
            assert!(ready.stderr.is_empty(), "{event}: {ready:?}");
        }
    });
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
    let holder = hold_the_slot(&rig);
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
    let holder = hold_the_slot(&rig);
    let mut client = LocalClient::connect(&rig.socket).expect("connect succeeds at the cap");
    assert!(matches!(client.recv(), Err(ClientError::Closed)));
    drop(holder);
}

#[test]
fn a_call_after_the_full_cap_close_stays_unanswered() {
    let rig = paused_rig();
    let holder = hold_the_slot(&rig);
    let params = hook_params(&rig);
    let mut client = LocalClient::connect(&rig.socket).expect("connect succeeds at the cap");
    assert!(matches!(client.recv(), Err(ClientError::Closed)));
    unanswered_shape(client.call(Method::HookEvent, Some(params)));
    drop(holder);
}

#[test]
fn a_peer_the_daemon_could_not_place_has_its_control_steps_blocked() {
    let rig = rig();
    rig.resolver.refuse.store(true, Ordering::SeqCst);
    for event in CONTROL {
        let output = run_hook_with_a_free_slot(&rig, &rig.token, event);
        assert_eq!(output.status.code(), Some(2), "{event}: {output:?}");
        assert_eq!(output.stderr, BLOCK);
    }
    drop(rig.running);
}
