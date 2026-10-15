//! Native Windows acceptance for the local control plane.
//!
//! The cross-check proves the types exist; these tests exercise the kernel boundaries it cannot:
//! the pipe owner and DACL read from the accepted server handle, operator admission over the
//! shipped classifier, and a stopped serve loop returning while an idle peer is still connected.
#![cfg(windows)]

use std::num::NonZeroUsize;
use std::os::windows::fs::symlink_file;
use std::os::windows::io::AsHandle as _;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::rpc::cred::{OPERATOR_CREDENTIAL_LEN, OperatorCredential};
use dudes_proto::rpc::{ErrorCode, Method};
use dudes_rpc::auth::PeerProvenance;
use dudes_rpc::testing::{AdmitAll, FakeResolver};
use dudes_rpc::{
    Dispatcher, Listener as _, LocalClient, LocalTransport, Requirement, StopFlag, serve,
};
use serde_json::json;

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

const RETURN_BOUND: Duration = Duration::from_secs(5);

fn credential() -> OperatorCredential {
    OperatorCredential::new("A".repeat(OPERATOR_CREDENTIAL_LEN)).expect("valid credential")
}

fn protected_runtime() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().expect("temporary root");
    let runtime = root.path().join("run");
    dudes_win::create_runtime_dir_creator_only(&runtime).expect("protected runtime directory");
    (root, runtime)
}

fn refused_existing_credential(runtime: &std::path::Path, context: &str) -> std::io::Error {
    match LocalTransport::bind_in_with_credential_and_halt(
        runtime,
        "duded.sock",
        &credential(),
        dudes_host::disconnect_named_pipe,
    ) {
        Err(error) => error,
        Ok(transport) => panic!(
            "{context}: CREATE_NEW must refuse the existing credential unchanged: {transport:?}"
        ),
    }
}

/// The pipe applies the captured `TokenUser` SID as both owner and sole DACL trustee.
#[test]
fn accepted_pipe_has_creator_only_owner_and_dacl() {
    let root = tempfile::tempdir().expect("temporary root");
    let runtime = root.path().join("run");
    let sid = dudes_win::current_user_sid().expect("capture current TokenUser SID before bind");
    let transport = LocalTransport::bind_in_with_credential_and_halt(
        &runtime,
        "duded.sock",
        &credential(),
        dudes_host::disconnect_named_pipe,
    )
    .expect("bind protected pipe");
    let endpoint = transport.path().to_path_buf();
    let client = std::thread::spawn(move || LocalClient::connect(endpoint).expect("connect"));
    let (connection, peer) = transport.accept().expect("accept client");

    assert_eq!(peer.pid.reported(), i32::try_from(std::process::id()).ok());
    assert_eq!(
        dudes_win::read_owner(connection.as_handle()).expect("read pipe owner"),
        sid
    );
    assert_eq!(
        dudes_win::read_dacl(connection.as_handle()).expect("read pipe DACL"),
        dudes_win::Dacl::Present {
            allowed: vec![sid],
            denied: Vec::new(),
            unread: 0,
        }
    );

    drop(connection);
    drop(client.join().expect("client thread"));
}

/// Ping and an operator-only method both traverse the shipped Windows door.
#[test]
fn daemon_answers_ping_and_admits_a_presented_operator_credential() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let endpoint = daemon.socket().to_path_buf();
    let text = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("read minted credential");
    let mut running = common::start(daemon);
    let mut client = LocalClient::connect(&endpoint).expect("connect to daemon");

    let ping = client
        .call(Method::SystemPing, None)
        .expect("system.ping answered");
    assert!(ping.error.is_none(), "system.ping failed: {:?}", ping.error);

    let why = client
        .call_as_operator(
            Method::AgentWhy,
            Some(json!({ "agent": "01ARZ3NDEKTSV4RRFFQ69G5FAV" })),
            OperatorCredential::new(text).expect("minted credential is valid"),
        )
        .expect("agent.why answered");
    assert_ne!(
        why.error.as_ref().map(|error| error.code),
        Some(ErrorCode::Unauthorized.code()),
        "the presented credential must pass the Windows operator door: {why:?}"
    );

    drop(client);
    assert_eq!(running.stop(ExitCause::Ended), ExitCause::Ended);
}

/// Lexical runtime aliases derive different pipe names but resolve one credential path. The file's
/// retained `CREATE_NEW` handle refuses the alias daemon without changing the live bytes or door.
#[test]
fn a_runtime_alias_cannot_replace_a_live_daemons_credential() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot first daemon");
    let endpoint = daemon.socket().to_path_buf();
    let mut running = common::start(daemon);
    let credential_path = settings
        .runtime_dir
        .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME);
    let bytes = std::fs::read(&credential_path).expect("first credential bytes");

    let alias_component = fixture.runtime.path().join("alias");
    dudes_win::create_runtime_dir_creator_only(&alias_component)
        .expect("creator-only alias component");
    let alias = alias_component.join("..").join("run");
    assert_ne!(
        dudes_rpc::local_endpoint(&settings.runtime_dir, &settings.socket_name),
        dudes_rpc::local_endpoint(&alias, &settings.socket_name),
        "positive control: the lexical aliases must hash to different pipe names"
    );
    LocalTransport::bind_in_with_credential_and_halt(
        &alias,
        &settings.socket_name,
        &credential(),
        dudes_host::disconnect_named_pipe,
    )
    .expect_err("the shared credential namespace refuses the alias daemon");
    assert_eq!(std::fs::read(&credential_path).unwrap(), bytes);
    assert!(dudes_win::is_creator_only(&credential_path).expect("credential descriptor"));

    let mut client = LocalClient::connect(&endpoint).expect("first daemon remains reachable");
    let ping = client
        .call(Method::SystemPing, None)
        .expect("first daemon still answers");
    assert!(ping.error.is_none(), "first daemon ping failed: {ping:?}");
    drop(client);
    running.stop(ExitCause::Ended);
}

/// No entry the daemon did not create is inspected, replaced or deleted. Existing and dangling
/// reparse arms separately preserve both the link and its target state.
#[test]
fn a_preexisting_windows_credential_is_never_reclaimed() {
    for contents in [
        "A".repeat(OPERATOR_CREDENTIAL_LEN),
        "malformed credential".to_owned(),
    ] {
        let (_root, runtime) = protected_runtime();
        let path = runtime.join(dudes_rpc::OPERATOR_CREDENTIAL_NAME);
        std::fs::write(&path, &contents).expect("plant regular credential");
        refused_existing_credential(&runtime, "regular credential");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
    }

    let (_root, runtime) = protected_runtime();
    let path = runtime.join(dudes_rpc::OPERATOR_CREDENTIAL_NAME);
    std::fs::create_dir(&path).expect("plant directory entry");
    refused_existing_credential(&runtime, "directory credential");
    assert!(path.is_dir(), "the pre-existing directory was changed");

    let (root, runtime) = protected_runtime();
    let path = runtime.join(dudes_rpc::OPERATOR_CREDENTIAL_NAME);
    let target = root.path().join("existing-target");
    std::fs::write(&target, "target bytes").expect("plant existing target");
    symlink_file(&target, &path).expect("plant existing-target reparse entry");
    refused_existing_credential(&runtime, "existing-target reparse credential");
    assert!(
        std::fs::symlink_metadata(&path)
            .expect("existing-target link")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_link(&path).expect("existing-target link target"),
        target
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "target bytes");

    let (root, runtime) = protected_runtime();
    let path = runtime.join(dudes_rpc::OPERATOR_CREDENTIAL_NAME);
    let target = root.path().join("dangling-target");
    symlink_file(&target, &path).expect("plant dangling reparse entry");
    refused_existing_credential(&runtime, "dangling-target reparse credential");
    assert!(
        std::fs::symlink_metadata(&path)
            .expect("dangling link")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_link(&path).expect("dangling link target"),
        target
    );
    assert!(
        !target.exists(),
        "the dangling target was created or changed"
    );
}

/// A compatible reader may outlive the daemon. It delays deletion and therefore keeps restart
/// fail-closed; its last close removes the path and permits the same-name retry.
#[test]
fn a_reader_delays_credential_deletion_without_permitting_takeover() {
    let (_root, runtime) = protected_runtime();
    let transport = LocalTransport::bind_in_with_credential_and_halt(
        &runtime,
        "duded.sock",
        &credential(),
        dudes_host::disconnect_named_pipe,
    )
    .expect("first transport");
    let path = runtime.join(dudes_rpc::OPERATOR_CREDENTIAL_NAME);
    let reader = std::fs::File::open(&path).expect("compatible held reader");
    drop(transport);

    refused_existing_credential(&runtime, "reader-delayed credential");
    drop(reader);
    assert!(
        !path.exists(),
        "last compatible close did not remove credential"
    );

    let retry = LocalTransport::bind_in_with_credential_and_halt(
        &runtime,
        "duded.sock",
        &credential(),
        dudes_host::disconnect_named_pipe,
    )
    .expect("same-name retry after last reader close");
    drop(retry);
    assert!(!path.exists());
}

/// A state-changing halt releases an idle connection and lets `serve` return within a bound.
#[test]
fn stop_returns_with_an_idle_pipe_peer() {
    let root = tempfile::tempdir().expect("temporary root");
    let runtime = root.path().join("run");
    let transport = Arc::new(
        LocalTransport::bind_in_with_credential_and_halt(
            &runtime,
            "duded.sock",
            &credential(),
            dudes_host::disconnect_named_pipe,
        )
        .expect("bind protected pipe"),
    );
    let endpoint = transport.path().to_path_buf();
    let stop = Arc::new(StopFlag::new());
    let resolver = Arc::new(FakeResolver::new(PeerProvenance::Unresolvable));
    let mut dispatcher = Dispatcher::new(AdmitAll);
    dispatcher.register(Method::SystemPing, Requirement::AnyRole, |_| {
        Ok(json!({ "pong": true }))
    });
    let (finished, returned) = mpsc::channel();
    let served = Arc::clone(&transport);
    let stopping = Arc::clone(&stop);
    std::thread::spawn(move || {
        let result = serve(
            &*served,
            &*resolver,
            &dispatcher,
            &stopping,
            NonZeroUsize::new(4).expect("nonzero"),
        );
        let _ = finished.send(result);
    });

    let idle = LocalClient::connect(&endpoint).expect("connect idle peer");
    let deadline = std::time::Instant::now() + RETURN_BOUND;
    while stop.live_connections() != 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "idle peer was not registered before the bound"
        );
        std::thread::yield_now();
    }
    stop.stop();
    // The stopping server may close this wake-only connection before client inspection finishes.
    // The bounded successful serve result below is the shutdown verdict, including in that race.
    let wake = LocalClient::connect(&endpoint);

    returned
        .recv_timeout(RETURN_BOUND)
        .expect("serve did not return after the pipe was halted")
        .expect("serve returned an error");
    drop(wake);
    drop(idle);
}
