//! The shutdown exit path over the **wire**, and the operator surface that reaches it.
//!
//! # Why this file exists, and what it runs against
//!
//! This file does not hand the daemon a hand-rolled authorizer: `duded::boot` builds the shipped `PeerClassifier` from the credential it mints and the
//! token projection its loop publishes, so what runs below is the daemon's own seam and the
//! daemon's own credential — the difference between *"the classifier admitted an operator"* and
//! *"a fixture said it would"*.
//!
//! Nothing here selects a custom peer-classification column: this fixture deliberately passes fixed `NoWalk`,
//! so every peer is column 3 and a presented credential is row 1 × column 3 → `Operator`. The
//! production process resolver has its own real-host socket coverage.
//!
//! This file is the check on `system.shutdown`: that it is registered, that its requirement is
//! `Operator`, and that its handler answers before the door closes. Every other driving test
//! reaches the loop through `Handle::ask`, which bypasses the dispatcher and the seam entirely.
//!
//! # What is not covered
//!
//! The **separate-process** arm. `main.rs` hard-codes the Claude adapter whose command is
//! `claude`, so a real `duded` on a box without that program cannot produce an agent child to
//! assert gone.
#![cfg(unix)]

use std::sync::Arc;

use duded::startup::{TeardownStep, Witness};
use duded::{ExitCause, boot};
use dudes_proto::Record;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::{Empty, ErrorCode, Method, RefusalGate, RefusedData};
use dudes_rpc::Client;
use dudes_supervisor::{Input, host::Liveness};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

/// The credential this daemon minted, read the way an operator's CLI would.
fn credential_of(settings: &duded::Settings) -> OperatorCredential {
    let text = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("the daemon minted a credential");
    OperatorCredential::new(text).expect("it is well formed")
}

/// The whole operator path, over one socket: hire, set a goal, be refused without a credential, and
/// stop the daemon with one.
///
/// One test rather than four, because the arms share a daemon and the *order* is part of what is
/// asserted — the refusal has to leave the daemon still serving, which is only observable if
/// something asks it afterwards.
#[test]
fn the_operator_surface_answers_over_the_socket_and_shutdown_needs_the_credential() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::new();
    let runner = Arc::new(ScriptedRunner::default());
    let witness = Witness::default();
    let daemon = boot(
        &settings,
        seams(
            host.clone(),
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            witness.clone(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    let socket = settings.socket();
    let mut client = Client::connect(&socket).expect("connect to the daemon");

    // --- the operator surface answers ----------------------------------------------------------
    hire_and_set_a_goal(&mut client, &settings, &fixture);

    let pids = host.pids();
    assert_eq!(
        pids.len(),
        1,
        "the goal started a turn, so there is a child"
    );
    assert_eq!(
        host.alive_of(pids[0]),
        Liveness::Alive,
        "control: it is alive before the shutdown"
    );

    // --- the requirement is Operator, not AnyRole ----------------------------------------------
    // Without this arm, `system.shutdown` registered `AnyRole` would pass every other assertion in
    // this file.
    let refused = client
        .call(Method::SystemShutdown, None)
        .expect("the call is answered, with an error");
    assert!(
        refused.result.is_none() && refused.error.is_some(),
        "system.shutdown with no credential must be refused: {refused:?}"
    );

    // And the refusal left the daemon serving — which is what makes it a refusal rather than a stop.
    let ping = client
        .call(Method::SystemPing, None)
        .expect("system.ping is still answered");
    assert!(
        ping.error.is_none(),
        "the daemon is still up after the refusal: {:?}",
        ping.error
    );
    assert_eq!(host.shutdowns(), 0, "and nothing has swept");

    // --- the real exit path, over the wire ------------------------------------------------------
    let stopped = client
        .call_as_operator(Method::SystemShutdown, None, credential_of(&settings))
        .expect("system.shutdown is answered");
    assert!(
        stopped.error.is_none(),
        "system.shutdown with the credential: {:?}",
        stopped.error
    );
    // The answer arrived, which is the handler's documented ordering: the exit sequence stops the
    // transport last, so an operator's call is answered before the door closes.

    let cause = running.join();
    assert_eq!(
        cause,
        ExitCause::ShutdownRpc,
        "the daemon stopped for the reason the handler gave"
    );
    assert_eq!(host.shutdowns(), 1, "exactly once");
    assert_eq!(
        host.alive_of(pids[0]),
        Liveness::Gone,
        "and the child it launched is gone"
    );
    assert_eq!(runner.sweeps(), 1);
    assert_eq!(witness.teardown.steps(), TeardownStep::ORDER.to_vec());
}

/// `agent.hire` then `goal.set`, both over the socket and both presenting the credential.
///
/// Split out for length. What it asserts along the way is `Control`'s own contract: the answer comes
/// back only after the record is durable, so the journal already carries the `AgentHired` by the
/// time the response is read.
fn hire_and_set_a_goal(
    client: &mut Client<
        std::io::BufReader<std::os::unix::net::UnixStream>,
        std::os::unix::net::UnixStream,
    >,
    settings: &duded::Settings,
    fixture: &Fixture,
) -> (dudes_types::AgentId, dudes_types::TurnId) {
    let hire = client
        .call_as_operator(
            Method::AgentHire,
            Some(serde_json::to_value(common::hire_params(fixture.state.path())).expect("params")),
            credential_of(settings),
        )
        .expect("agent.hire is answered");
    assert!(
        hire.error.is_none(),
        "agent.hire over the wire: {:?}",
        hire.error
    );
    let agent: dudes_proto::rpc::agent::HireResult =
        serde_json::from_value(hire.result.expect("a result")).expect("a hire result");
    assert!(
        fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::AgentHired { .. })),
        "and the record is durable before the answer went out"
    );

    let goal = client
        .call_as_operator(
            Method::GoalSet,
            Some(
                serde_json::to_value(common::goal_params(agent.agent, &["/usr/bin/true"]))
                    .expect("params"),
            ),
            credential_of(settings),
        )
        .expect("goal.set is answered");
    assert!(goal.error.is_none(), "goal.set: {:?}", goal.error);
    let turn = fixture.await_turn();
    fixture.await_record("the operator goal's TurnSpawned", |record| {
        matches!(record, Record::TurnSpawned { turn: found, .. } if *found == turn).then_some(())
    });
    (agent.agent, turn)
}

/// **The refusing half: the same daemon, presenting nothing, is refused
/// the operator surface.**
///
/// This is the control for the test above, and the pairing is what makes either one evidence.
/// Same binary path, same registrations, same classifier, same credential file on disk — the only
/// difference is whether the call presents the credential. A refusal that came from an
/// unregistered method could not survive it, because the arm above is admitted through those very
/// registrations.
///
/// `system.ping` answers, which is a method with an any-role `Requirement`: a peer that presented nothing is
/// `Unproven`, a role rather than a closed door, so the daemon stays probeable. The two operator
/// methods are refused by their own `Requirement`, after the seam admitted the call — so the
/// requirement is load-bearing here and a change to it is red.
#[test]
fn the_shipped_default_serves_ping_and_refuses_the_operator_surface() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        // `seams`, not `seams_with`: the shipped resolver and the shipped classifier, exactly
        // as `main.rs` composes them.
        common::seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    let mut client = Client::connect(settings.socket()).expect("connect");
    let ping = client.call(Method::SystemPing, None).expect("answered");
    assert!(ping.error.is_none(), "control: ping is admitted");

    for method in [Method::SystemShutdown, Method::AgentHire] {
        let response = client.call(method, None).expect("answered with an error");
        assert!(
            response.error.is_some(),
            "{} presenting no credential must be refused: {response:?}",
            method.name()
        );
    }

    running.stop(ExitCause::Ended);
}

/// `agent.retire` crosses the production socket, refuses the one liveness state that cannot be
/// acted on, and answers only after the exact retirement is durable.
#[test]
fn agent_retire_is_routed_over_the_socket_and_journals_agent_retired() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let mut client = Client::connect(settings.socket()).expect("connect");
    let (agent, turn) = hire_and_set_a_goal(&mut client, &settings, &fixture);
    fixture.await_record("TurnSpawned", |record| match record {
        Record::TurnSpawned { turn: spawned, .. } if *spawned == turn => Some(()),
        _ => None,
    });

    let reason = "the quarter ended";
    let params = || {
        Some(serde_json::json!({
            "agent": agent,
            "reason": reason,
        }))
    };

    let unauthorized = client
        .call(Method::AgentRetire, params())
        .expect("the unauthenticated call is answered");
    assert_eq!(
        unauthorized.error.expect("an error").code,
        ErrorCode::Unauthorized.code(),
        "this method's `Requirement` is Operator-only"
    );
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::AgentRetired { .. })),
        "a refused caller retires nobody"
    );

    running.handle.input(Input::Liveness {
        turn,
        liveness: Liveness::Unknown,
    });
    running.handle.doctor().expect("order the Unknown input");
    let unknown = client
        .call_as_operator(Method::AgentRetire, params(), credential_of(&settings))
        .expect("the Unknown-liveness call is answered");
    let error = unknown.error.expect("Unknown liveness refuses retirement");
    assert_eq!(error.code, ErrorCode::Refused.code());
    let data: RefusedData =
        serde_json::from_value(error.data.expect("typed refusal")).expect("RefusedData");
    assert_eq!(data.gate, RefusalGate::LivenessUnknown);
    let records = fixture.records();
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::AgentRetired { .. })),
        "the liveness refusal happens before the retirement record"
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::KillDeferred { .. })),
        "retirement is refused rather than deferred"
    );

    running.handle.input(Input::Liveness {
        turn,
        liveness: Liveness::Alive,
    });
    running.handle.doctor().expect("order the Alive input");
    let retired = client
        .call_as_operator(Method::AgentRetire, params(), credential_of(&settings))
        .expect("the Alive-liveness call is answered");
    let result: Empty = serde_json::from_value(
        retired
            .result
            .unwrap_or_else(|| panic!("agent.retire returned no result: {:?}", retired.error)),
    )
    .expect("the declared Empty result");
    assert_eq!(result, Empty {});

    let exact: Vec<_> = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::AgentRetired { id, reason, .. } if id == agent => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(
        exact,
        vec![reason.to_owned()],
        "the response follows one durable retirement carrying the exact request reason"
    );

    running.stop(ExitCause::Ended);
}

#[test]
fn local_hire_appearance_is_finite_recorded_and_replayable() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let mut client = Client::connect(settings.socket()).expect("operator socket");
    let raw = serde_json::to_value(common::hire_params(fixture.state.path())).unwrap();
    let before = fixture.records();
    for (field, value) in [
        ("character", "a"),
        ("character", "../cast"),
        ("accent", "red"),
        ("accent", "#fff"),
    ] {
        let mut unknown = raw.clone();
        unknown[field] = serde_json::json!(value);
        let response = client
            .call_as_operator(Method::AgentHire, Some(unknown), credential_of(&settings))
            .unwrap();
        assert_eq!(
            response.error.unwrap().code,
            ErrorCode::InvalidParams.code()
        );
        assert_eq!(
            fixture.records(),
            before,
            "unknown {field} appended a record"
        );
    }
    let mut ids = Vec::new();
    for (character, accent) in [
        (None, None),
        (Some("h"), None),
        (None, Some("sky")),
        (Some("h"), Some("sky")),
    ] {
        let mut request = raw.clone();
        if let Some(character) = character {
            request["character"] = serde_json::json!(character);
        }
        if let Some(accent) = accent {
            request["accent"] = serde_json::json!(accent);
        }
        let response = client
            .call_as_operator(Method::AgentHire, Some(request), credential_of(&settings))
            .unwrap();
        assert!(response.error.is_none(), "{:?}", response.error);
        let hired: dudes_proto::rpc::agent::HireResult =
            serde_json::from_value(response.result.unwrap()).unwrap();
        ids.push((
            hired.agent,
            character.map(|_| dudes_types::CastCharacter::H),
            accent.map(|_| dudes_types::AccentColour::Sky),
        ));
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    let reader = dudes_journal::Reader::new(&settings.state_dir.join("journal"));
    let mut projections =
        dudes_journal::Projections::open(&fixture.state.path().join("appearance.redb")).unwrap();
    dudes_journal::replay(&reader, &mut projections).unwrap();
    let mut world = dudes_supervisor::world::World::default();
    for record in reader.iter_from(dudes_types::Lsn::ZERO).unwrap() {
        let (lsn, entry) = record.unwrap();
        world.apply(lsn, &entry);
    }
    for (id, character, accent) in ids {
        let projected = projections.agent(id).unwrap().unwrap();
        assert_eq!((projected.character, projected.accent), (character, accent));
        let replayed = &world.agents[&id];
        assert_eq!((replayed.character, replayed.accent), (character, accent));
        assert!(fixture.records().iter().any(|record| matches!(record,
            Record::AgentHired { id: recorded, character: c, accent: a, .. }
            if *recorded == id && *c == character && *a == accent)));
    }
}
