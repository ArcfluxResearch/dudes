//! The real shim against a real daemon whose connection cap is full: a control step is refused
//! because the daemon accepted the connection and ended it without an answer.

mod common;

use std::num::NonZeroUsize;
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
use dudes_proto::rpc::Method;
use dudes_proto::rpc::hook::{HookEventParams, HookToken};
use dudes_rpc::auth::{PeerIdentity, PeerResolution, PeerResolver};
use dudes_rpc::{ClientError, FrameError, LocalClient};
use dudes_types::AgentId;
use serde_json::Value;

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
        rpc_connections,
    }
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

fn paused_rig() -> Rig {
    let rig = rig();
    assert!(matches!(
        rig.running.handle.ask(Ask::Pause(rig.agent)),
        Ok(duded::ports::Answered::Done)
    ));
    rig
}

fn connect_at_full_cap(rig: &Rig) -> Result<LocalClient, ClientError> {
    LocalClient::connect(&rig.socket).map_err(|error| {
        assert!(
            cfg!(windows) && error.raw_os_error() == Some(233),
            "the saturated connection failed before setup with {error:?}"
        );
        ClientError::Io(error)
    })
}

fn assert_unanswered<T: std::fmt::Debug>(result: Result<T, ClientError>) {
    use std::io::ErrorKind::{BrokenPipe, ConnectionReset, NotConnected, UnexpectedEof};

    let (label, error) = match result {
        Err(ClientError::Closed) => return,
        Err(ClientError::Frame(FrameError::Io(e))) => ("Frame(Io)", e),
        Err(ClientError::Io(e)) => ("Io", e),
        other => panic!("the cap-full call produced {other:?}"),
    };
    assert!(
        matches!(
            error.kind(),
            BrokenPipe | NotConnected | ConnectionReset | UnexpectedEof
        ) || (cfg!(windows) && error.raw_os_error() == Some(233)),
        "unclassified {label} error: kind {:?}, raw OS code {:?}",
        error.kind(),
        error.raw_os_error()
    );
}

fn hook_params(rig: &Rig) -> Value {
    serde_json::to_value(HookEventParams {
        token: HookToken::new(rig.token.expose()).unwrap(),
        raw: payload("PreToolUse"),
    })
    .unwrap()
}

/// A saturated pipe may close during Windows client setup or during the call. Every outcome must
/// still be one the shim blocks on, and none may be an answer.
#[test]
fn a_full_cap_never_answers_and_ends_in_a_shape_the_shim_blocks_on() {
    let rig = paused_rig();
    let holder = hold_the_slot(&rig);
    let params = hook_params(&rig);
    for _ in 0..RUNS {
        assert_unanswered(
            connect_at_full_cap(&rig)
                .and_then(|mut client| client.call(Method::HookEvent, Some(params.clone()))),
        );
    }
    drop(holder);
}

#[test]
fn a_full_cap_ends_a_silent_connection_without_an_answer() {
    let rig = paused_rig();
    let holder = hold_the_slot(&rig);
    assert_unanswered(connect_at_full_cap(&rig).and_then(|mut client| {
        let result = client.recv();
        assert!(matches!(result, Err(ClientError::Closed)), "{result:?}");
        result
    }));
    drop(holder);
}

#[test]
fn a_call_after_the_full_cap_close_stays_unanswered() {
    let rig = paused_rig();
    let holder = hold_the_slot(&rig);
    let params = hook_params(&rig);
    assert_unanswered(connect_at_full_cap(&rig).and_then(|mut client| {
        let result = client.recv();
        assert!(matches!(result, Err(ClientError::Closed)), "{result:?}");
        client.call(Method::HookEvent, Some(params))
    }));
    drop(holder);
}

#[test]
fn a_full_cap_blocks_real_control_hooks_but_an_absent_daemon_does_not() {
    const UNANSWERED_BLOCK: &[u8] = b"dudes-hook: daemon connection ended before an answer; control step blocked; retry, and restart duded if this persists\n";
    const FAILURE: &[u8] = b"dudes-hook: hook delivery failed\n";
    let rig = paused_rig();
    let holder = hold_the_slot(&rig);
    let generated = generated_handlers(rig.agent, rig.fixture.state.path());
    for (event, expected) in [("PreToolUse", 2), ("UserPromptSubmit", 2), ("Stop", 0)] {
        let (_, output) = spawn_handler(
            handler(&generated, event),
            &rig.socket,
            &rig.token,
            &payload(event),
        );
        assert_eq!(output.status.code(), Some(expected), "{event}: {output:?}");
        assert!(output.stdout.is_empty(), "{event}: {output:?}");
        assert_eq!(
            output.stderr,
            if expected == 2 {
                UNANSWERED_BLOCK
            } else {
                FAILURE
            },
            "{event}"
        );
    }
    let missing = dudes_rpc::local_endpoint(rig.fixture.state.path(), "absent.sock");
    for event in ["PreToolUse", "UserPromptSubmit"] {
        let (_, output) = spawn_handler(
            handler(&generated, event),
            &missing,
            &rig.token,
            &payload(event),
        );
        assert_eq!(output.status.code(), Some(0), "{event}: {output:?}");
        assert!(output.stdout.is_empty(), "{event}: {output:?}");
        assert_eq!(output.stderr, FAILURE, "{event}");
    }
    drop(holder);
}
