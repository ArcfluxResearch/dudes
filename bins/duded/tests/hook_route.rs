//! Claude's ten generated handlers through the real shim and the daemon's one local route.

mod common;

use std::num::NonZeroUsize;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use common::hook_shim::{
    RecordingResolver, generated_handlers, handler, minted_token, payload, spawn_handler,
};
use common::{
    Fixture, ScriptedRunner, SharedHost, TestClock, goal_params, hire, seams_with, set_goal,
};
use duded::ports::{Handle, Job};
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::reply::{HookReply, VendorReply};
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::hook::HookEventParams;
use dudes_proto::rpc::{ErrorCode, Method};
use dudes_proto::{Event, EventKind, LiveActivityKind, PeerProvenance, Record};
use dudes_rpc::auth::{PeerClassifier, PeerResolver};
use dudes_rpc::testing::FakeMintedTokens;
use dudes_rpc::{LocalClient, Outbound, Subscribers};
use dudes_supervisor::{Input, RuntimeSignal};
use dudes_types::{AgentId, Provenance, Timestamp};
use serde_json::json;

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
);

fn floor_fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("Floor fixture");
    std::fs::create_dir(root.path().join("assets")).expect("assets");
    for path in [
        "index.html",
        "release-manifest.json",
        "assets/app-01234567.js",
    ] {
        std::fs::copy(
            std::path::Path::new(FLOOR_FIXTURE).join(path),
            root.path().join(path),
        )
        .expect("copy");
    }
    root
}

fn activity_after_one_pre_tool(floor: Option<&std::path::Path>) -> Vec<LiveActivityKind> {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.floor = floor.map(|assets| duded::settings::FloorSettings {
        assets_dir: assets.to_path_buf(),
        port: Some(0),
    });
    let resolver = RecordingResolver::new(PeerProvenance::Unresolvable);
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
    let socket = daemon.socket().to_path_buf();
    let mut running = common::start(daemon);
    let agent = hire(&running.handle, fixture.state.path());
    set_goal(
        &running.handle,
        goal_params(agent, &[common::true_program()]),
    );
    let token = minted_token(&fixture, &tokens, agent);
    fixture.await_record("the hook subject's actual managed spawn", |record| {
        matches!(record, Record::TurnSpawned { agent: owner, .. } if *owner == agent).then_some(())
    });
    running
        .handle
        .doctor()
        .expect("finish the hook subject's setup pump");
    resolver.set(PeerProvenance::Agent(agent));
    let response = LocalClient::connect(&socket)
        .expect("connect the hook peer")
        .call(
            Method::HookEvent,
            Some(
                serde_json::to_value(HookEventParams {
                    token,
                    raw: payload("PreToolUse"),
                })
                .unwrap(),
            ),
        )
        .expect("the hook is answered");
    assert!(response.error.is_none(), "{response:?}");
    if floor.is_some() {
        fixture.await_record("LiveActivityObserved", |record| {
            matches!(record, Record::LiveActivityObserved { .. }).then_some(())
        });
    } else {
        // Two sampler periods: a sampler started without the Floor would have flushed by now.
        std::thread::sleep(Duration::from_millis(2_500));
    }
    assert_eq!(running.stop(ExitCause::Ended), ExitCause::Ended);
    fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::LiveActivityObserved { kind, .. } => Some(kind),
            _ => None,
        })
        .collect()
}

#[test]
fn only_a_daemon_serving_the_floor_journals_hook_activity() {
    let floor = floor_fixture();
    assert_eq!(
        activity_after_one_pre_tool(Some(floor.path())),
        vec![LiveActivityKind::UsingTool]
    );
    assert_eq!(
        activity_after_one_pre_tool(None),
        Vec::<LiveActivityKind>::new()
    );
}

#[test]
fn a_sibling_process_with_a_stolen_hook_token_is_refused_without_effect() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let resolver = RecordingResolver::new(PeerProvenance::Unresolvable);
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
    let socket = daemon.socket().to_path_buf();
    let mut running = common::start(daemon);
    let alpha = hire(&running.handle, fixture.state.path());
    set_goal(
        &running.handle,
        goal_params(alpha, &[common::true_program()]),
    );
    let stolen = minted_token(&fixture, &tokens, alpha);
    let bravo = hire(&running.handle, fixture.state.path());
    resolver.set(PeerProvenance::Agent(bravo));

    let generated = generated_handlers(alpha, fixture.state.path());
    let before = fixture.records();
    let (pid, output) = spawn_handler(
        handler(&generated, "Stop"),
        &socket,
        &stolen,
        &payload("Stop"),
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "shim fails open on RPC refusal"
    );
    assert!(
        output.stdout.is_empty(),
        "an unauthorized call has no vendor body"
    );
    assert_eq!(output.stderr, b"dudes-hook: hook delivery failed\n");
    assert_eq!(resolver.pids().first().copied(), i32::try_from(pid).ok());

    let mut client = LocalClient::connect(&socket).expect("connect the refused peer");
    let response = client
        .call(
            Method::HookEvent,
            Some(
                serde_json::to_value(HookEventParams {
                    token: stolen,
                    raw: payload("Stop"),
                })
                .unwrap(),
            ),
        )
        .expect("the refusal is an answer");
    assert_eq!(
        response.error.as_ref().map(|error| error.code),
        Some(ErrorCode::Unauthorized.code())
    );
    assert!(
        response.result.is_none(),
        "Unauthorized carries no VendorReply"
    );
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::TurnEnded { .. }))
            .count(),
        before
            .iter()
            .filter(|record| matches!(record, Record::TurnEnded { .. }))
            .count(),
        "the forged Stop produced no TurnEnded"
    );
    assert_eq!(running.stop(ExitCause::Ended), ExitCause::Ended);
}

#[test]
fn stop_boundary_timeout_fails_open_and_queues_the_existing_signal() {
    for (event_name, kind) in [
        ("Stop", EventKind::TurnEnd { last_message: None }),
        ("SubagentStop", EventKind::SubagentEnd),
    ] {
        let fixture = Fixture::new();
        let (dispatcher, loop_side, agent) = direct_dispatcher_fast(&fixture);
        let waiting_dispatcher = Arc::clone(&dispatcher);
        let waiter =
            std::thread::spawn(move || direct_hook(&waiting_dispatcher, agent, event_name));
        let held = loop_side.recv().expect("hook queued before the wait");
        assert!(matches!(&held, Job::Hook { reply: Some(_), .. }));

        let response = waiter.join().expect("waiter thread");
        assert!(response.error.is_none(), "timeout fails open: {response:?}");
        let reply: VendorReply =
            serde_json::from_value(response.result.expect("timeout vendor reply")).unwrap();
        let expected = VendorReply {
            body: None,
            exit_code: 0,
        };
        assert_eq!(reply, expected, "timeout adds no vendor context");
        let encoded_event = Event {
            agent,
            at: Timestamp::from_unix_millis(1_789_000_000_000),
            provenance: Provenance::Observed,
            kind,
            raw: Some(payload(event_name)),
        };
        assert_ne!(
            dudes_providers::claude::encode_reply(
                &HookReply::allow_with("supervisor timeout"),
                &encoded_event,
            ),
            expected,
            "the former context-injecting timeout mutation must fail"
        );
        let signal = loop_side
            .recv()
            .expect("timeout signal queued after the hook");
        assert!(matches!(
            signal,
            Job::Input(input)
                if matches!(*input, Input::Signal {
                    agent: owner,
                    signal: RuntimeSignal::HookTimeout,
                } if owner == agent)
        ));
        drop(held);
    }
}

#[test]
fn admitted_control_deadlines_encode_denial_and_keep_the_original_job() {
    for (event, body) in [
        (
            "PreToolUse",
            json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "the daemon did not answer the control hook; retry, and restart duded if this persists"
            }}),
        ),
        (
            "UserPromptSubmit",
            json!({
                "decision":"block",
                "reason":"the daemon did not answer the control hook; retry, and restart duded if this persists"
            }),
        ),
    ] {
        let fixture = Fixture::new();
        let (dispatcher, queued, agent) = direct_dispatcher_fast(&fixture);
        let waiting = Arc::clone(&dispatcher);
        let caller = std::thread::spawn(move || direct_hook(&waiting, agent, event));
        let held = queued.recv_timeout(Duration::from_secs(1)).unwrap();
        let Job::Hook {
            event: original,
            reply: Some(reply),
            ..
        } = &held
        else {
            panic!("the original admitted hook")
        };
        assert_eq!(original.agent, agent);
        let mut expected_raw = payload(event);
        expected_raw["agent_id"] = json!(second_agent());
        assert_eq!(original.raw.as_ref(), Some(&expected_raw));
        let response = caller.join().unwrap();
        assert!(response.error.is_none(), "{response:?}");
        let received: VendorReply = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(
            received,
            VendorReply {
                body: Some(body),
                exit_code: 2
            }
        );
        assert!(
            reply.send(Ok(HookReply::ALLOW.into())).is_err(),
            "a late answer has no live caller"
        );
        assert!(matches!(
            queued.recv_timeout(Duration::from_secs(1)).unwrap(),
            Job::Input(input) if matches!(*input, Input::Signal {
                agent: seen,
                signal: RuntimeSignal::HookTimeout,
            } if seen == agent)
        ));
        assert!(matches!(queued.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let refused = direct_hook(&dispatcher, agent, event).error.unwrap();
        assert_eq!(
            refused.message,
            "hook event was not queued because hook capacity is full"
        );
        drop(held);
    }
}

fn direct_hook(
    dispatcher: &dudes_rpc::Dispatcher,
    agent: AgentId,
    event: &str,
) -> dudes_proto::rpc::Response {
    direct_hook_with_payload_owner(dispatcher, agent, event, second_agent())
}

fn direct_hook_with_payload_owner(
    dispatcher: &dudes_rpc::Dispatcher,
    agent: AgentId,
    event: &str,
    payload_owner: AgentId,
) -> dudes_proto::rpc::Response {
    let mut raw = payload(event);
    raw["agent_id"] = json!(payload_owner);
    let line = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "hook.event",
        "params": {
            "token": if agent == third_agent() {
                "DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD"
            } else if agent == second_agent() {
                "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"
            } else {
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            },
            "raw": raw
        }
    }))
    .unwrap();
    dispatcher.handle_line(
        &line,
        PeerProvenance::Agent(agent),
        &Arc::new(Outbound::sink()),
    )
}

fn second_agent() -> AgentId {
    AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap()
}

fn third_agent() -> AgentId {
    AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAX").unwrap()
}

fn direct_dispatcher(
    fixture: &Fixture,
) -> (Arc<dudes_rpc::Dispatcher>, mpsc::Receiver<Job>, AgentId) {
    direct_dispatcher_with_capacity(fixture, 1)
}

fn direct_dispatcher_fast(
    fixture: &Fixture,
) -> (Arc<dudes_rpc::Dispatcher>, mpsc::Receiver<Job>, AgentId) {
    direct_dispatcher_timed(fixture, 1, Duration::from_millis(100))
}

fn direct_dispatcher_with_capacity(
    fixture: &Fixture,
    capacity: usize,
) -> (Arc<dudes_rpc::Dispatcher>, mpsc::Receiver<Job>, AgentId) {
    direct_dispatcher_timed(fixture, capacity, duded::ports::DEFAULT_HOOK_REPLY_TIMEOUT)
}

fn direct_dispatcher_timed(
    fixture: &Fixture,
    capacity: usize,
    hook_reply_timeout: Duration,
) -> (Arc<dudes_rpc::Dispatcher>, mpsc::Receiver<Job>, AgentId) {
    let mut settings = fixture.settings();
    settings.max_connections = NonZeroUsize::new(capacity).unwrap();
    let agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let (jobs, loop_side) = mpsc::channel();
    let dispatcher = Arc::new(duded::boot::dispatch(
        &settings,
        PeerClassifier::new(
            OperatorCredential::new("C".repeat(43)).unwrap(),
            Arc::new(FakeMintedTokens::minted(&[
                ("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", agent),
                (
                    "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
                    second_agent(),
                ),
                ("DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD", third_agent()),
            ])),
        ),
        duded::hooks::Ingress::new(
            Handle::new(jobs).with_hook_reply_timeout(hook_reply_timeout),
            Arc::new(TestClock::new()),
        ),
        &fixture.journal_dir(),
        &Arc::new(Subscribers::new()),
        Arc::new(AtomicU64::new(0)),
        Arc::new(dudes_rpc::OperatorAuthoritySource::from_replayed_agents(
            false,
        )),
    ));
    (dispatcher, loop_side, agent)
}

#[test]
fn timed_out_control_keeps_full_pool_fail_closed_and_bounded() {
    let fixture = Fixture::new();
    let (dispatcher, loop_side, agent) = direct_dispatcher(&fixture);
    let first = direct_hook(&dispatcher, agent, "PreToolUse");
    assert!(
        first.error.is_none(),
        "the first caller receives a deadline reply"
    );

    let later: Vec<_> = ["PreToolUse", "UserPromptSubmit"]
        .into_iter()
        .flat_map(|event| (0..16).map(move |_| event))
        .map(|event| direct_hook(&dispatcher, second_agent(), event))
        .collect();
    let queued: Vec<_> = loop_side.try_iter().collect();
    for response in later {
        let error = response
            .error
            .expect("a full control pool refuses every call");
        assert_eq!(error.code, ErrorCode::InternalError.code());
        assert_eq!(
            error.message,
            "hook event was not queued because hook capacity is full"
        );
        assert!(response.result.is_none());
    }
    assert_eq!(queued.len(), 2, "one retained hook and its timeout signal");
    assert!(matches!(queued[0], Job::Hook { .. }));
    assert!(matches!(queued[1], Job::Input(_)));
    drop(queued);

    let waiting = Arc::clone(&dispatcher);
    let caller = std::thread::spawn(move || direct_hook(&waiting, second_agent(), "PreToolUse"));
    let resumed = loop_side
        .recv_timeout(Duration::from_secs(1))
        .expect("dropping retained work restores capacity");
    if let Job::Hook {
        reply: Some(reply), ..
    } = &resumed
    {
        reply.send(Ok(HookReply::ALLOW.into())).unwrap();
    } else {
        panic!("the newly admitted control job");
    }
    assert!(caller.join().expect("the resumed caller").error.is_none());
}

#[test]
fn one_agents_notifications_leave_capacity_for_another_agents_control() {
    let fixture = Fixture::new();
    let (dispatcher, loop_side, agent) = direct_dispatcher(&fixture);
    assert!(
        direct_hook(&dispatcher, agent, "Notification")
            .error
            .is_none()
    );
    let notification = loop_side.recv().expect("the admitted notification");
    let waiting = Arc::clone(&dispatcher);
    let caller = std::thread::spawn(move || direct_hook(&waiting, second_agent(), "PreToolUse"));
    let control = loop_side.recv_timeout(Duration::from_millis(500));
    if let Ok(Job::Hook {
        reply: Some(reply), ..
    }) = &control
    {
        reply
            .send(Ok(HookReply {
                decision: dudes_proto::reply::HookDecision::Deny {
                    reason: "the loop is paused".to_owned(),
                },
                context: None,
            }
            .into()))
            .unwrap();
    }
    let response = caller.join().expect("the control caller");
    drop(notification);
    assert!(
        response.error.is_none(),
        "notification flood denied control: {response:?}"
    );
    let reply: VendorReply = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(reply.exit_code, 2);
    assert_eq!(
        reply.body.unwrap()["hookSpecificOutput"]["permissionDecisionReason"],
        "the loop is paused"
    );
    assert!(
        control.is_ok(),
        "control entered the stalled queue's reserved capacity"
    );
}

#[test]
fn a_queued_hook_owns_the_only_permit_and_overload_queues_no_second_job() {
    let fixture = Fixture::new();
    let (dispatcher, loop_side, agent) = direct_dispatcher(&fixture);

    let first_dispatcher = Arc::clone(&dispatcher);
    let first = std::thread::spawn(move || direct_hook(&first_dispatcher, agent, "Stop"));
    let held = loop_side
        .recv_timeout(Duration::from_secs(1))
        .expect("the first hook job");
    assert!(matches!(&held, Job::Hook { .. }));

    let overloaded = direct_hook(&dispatcher, agent, "Stop");
    let error = overloaded.error.expect("the saturated hook is refused");
    assert_eq!(error.code, ErrorCode::InternalError.code());
    assert_eq!(
        error.message,
        "hook event was not queued because hook capacity is full"
    );
    assert!(overloaded.result.is_none());
    assert!(
        matches!(loop_side.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "overload must not enqueue a second job"
    );

    drop(held);
    assert!(first.join().unwrap().error.is_some());

    let third_dispatcher = Arc::clone(&dispatcher);
    let third = std::thread::spawn(move || direct_hook(&third_dispatcher, agent, "Stop"));
    let admitted_again = loop_side
        .recv_timeout(Duration::from_secs(1))
        .expect("dropping the first job returns capacity");
    assert!(matches!(&admitted_again, Job::Hook { .. }));
    drop(admitted_again);
    assert!(third.join().unwrap().error.is_some());
}
