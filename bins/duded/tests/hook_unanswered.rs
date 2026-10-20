//! The real shim against a real daemon whose connection cap is full: a control step is refused
//! because the daemon accepted the connection and ended it without an answer.

mod common;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use common::hook_shim::{minted_token, payload};
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
    _fixture: Fixture,
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
        _fixture: fixture,
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
