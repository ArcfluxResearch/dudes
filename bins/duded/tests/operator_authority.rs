//! Boot derives the operator-effect acquisition epoch only from the replayed journal.
#![cfg(unix)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::Record;
use dudes_proto::rpc::Method;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_rpc::Client;
use dudes_rpc::auth::PeerProvenance;

mod common;

use common::{Fixture, ScriptedColumn, ScriptedRunner, SharedHost, TestClock, seams, seams_with};

/// The credential this daemon minted, read through the same file an operator client uses.
fn credential_of(settings: &duded::Settings) -> OperatorCredential {
    let text = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("the daemon minted an operator credential");
    OperatorCredential::new(text).expect("the operator credential is well formed")
}

/// Wait for the accept loop's diagnostic count rather than racing its connection thread.
fn await_live_leases(source: &dudes_rpc::OperatorAuthoritySource, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while source.live_leases() != expected {
        assert!(
            Instant::now() < deadline,
            "authority lease count never became {expected}; current count is {}",
            source.live_leases()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn zero_replayed_agents_opens_and_any_replayed_agent_seals() {
    let fixture = Fixture::new();
    let settings = fixture.settings();

    let fresh = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("a zero-agent journal boots");
    assert!(
        !fresh.operator_authority().is_sealed(),
        "zero replayed agents is the positive control for the pre-agent epoch"
    );
    let mut fresh = common::start(fresh);
    common::hire(&fresh.handle, fixture.state.path());
    assert!(
        fixture
            .records()
            .iter()
            .any(|record| matches!(record, dudes_proto::Record::AgentHired { .. })),
        "the first run put an agent on the authoritative journal"
    );
    fresh.stop(ExitCause::Ended);

    let replayed = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("the journal with an agent replays");
    assert!(
        replayed.operator_authority().is_sealed(),
        "any recorded agent seals before the restarted listener can grant a lease"
    );
    let mut replayed = common::start(replayed);
    replayed.stop(ExitCause::Ended);
}

/// The production accept loop and the production hire port must share one source.
///
/// A test of `OperatorAuthoritySource` alone cannot prove that boot handed the same value to both
/// consumers. This one retains only the source's diagnostic handle, then drives the real socket
/// and daemon dispatcher. A pre-hire connection acquires before `agent.hire`; the hire seals the
/// source before delivery while leaving that lease live; and a connection accepted afterwards
/// cannot increase the count. Giving `start_accept` a distinct always-open source makes the first
/// count stay zero, while giving `LoopControl` a distinct source leaves this one unsealed.
#[test]
fn production_accept_and_hire_share_the_authority_source() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let column = ScriptedColumn::new(PeerProvenance::NoRecordedAgent);
    let daemon = boot(
        &settings,
        seams_with(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
            Arc::clone(&column) as Arc<dyn dudes_rpc::auth::PeerResolver>,
        ),
    )
    .expect("boot");
    let authority = Arc::clone(daemon.operator_authority());
    let mut running = common::start(daemon);

    let mut pre_hire = Client::connect(settings.socket()).expect("connect before hire");
    await_live_leases(&authority, 1);

    let hired = pre_hire
        .call_as_operator(
            Method::AgentHire,
            Some(
                serde_json::to_value(common::hire_params(fixture.state.path()))
                    .expect("hire params"),
            ),
            credential_of(&settings),
        )
        .expect("agent.hire is answered through the production dispatcher");
    assert!(hired.error.is_none(), "agent.hire: {:?}", hired.error);
    assert!(
        fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::AgentHired { .. })),
        "the production hire reached the durable journal before its answer"
    );
    assert!(
        authority.is_sealed(),
        "the production hire port sealed the source the accept loop uses"
    );
    assert_eq!(
        authority.live_leases(),
        1,
        "the pre-hire connection keeps its lease after sealing"
    );

    let mut post_hire = Client::connect(settings.socket()).expect("connect after hire");
    let ping = post_hire
        .call(Method::SystemPing, None)
        .expect("the post-hire connection is accepted and answered");
    assert!(ping.error.is_none(), "system.ping: {:?}", ping.error);
    assert_eq!(
        authority.live_leases(),
        1,
        "the accepted post-hire connection cannot acquire a second lease"
    );
    assert_eq!(
        column.asked(),
        2,
        "both socket connections reached the production accept boundary"
    );

    drop(post_hire);
    assert_eq!(
        authority.live_leases(),
        1,
        "closing an authority-free connection does not affect the surviving lease"
    );
    drop(pre_hire);
    await_live_leases(&authority, 0);
    running.stop(ExitCause::Ended);
}
