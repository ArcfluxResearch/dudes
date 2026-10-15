//! The operator's trust declaration at `agent.hire` is journaled
//! on `AgentHired` and decides the managed launch's setting sources.
//!
//! The hire goes over the socket as the operator, the way `dudes hire --trusted` sends it, so the
//! test runs through the door's requirement, the daemon's hire path, the journal on disk and the
//! real Claude adapter's argv. A hire that omits the key is the control: without it a daemon that
//! marked every hire trusted would pass.
#![cfg(unix)]

use std::ffi::{OsStr, OsString};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::Record;
use dudes_proto::rpc::agent::HireResult;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::{ErrorCode, Method};
use dudes_providers::adapter::Adapter;
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::codex::CodexAdapter;
use dudes_providers::pi::PiAdapter;
use dudes_providers::registry::Adapters;
use dudes_rpc::Client;
use dudes_types::Provider;

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

/// The value following `--setting-sources` in `argv`.
fn setting_sources(argv: &[OsString]) -> &OsStr {
    let at = argv
        .iter()
        .position(|part| part == "--setting-sources")
        .expect("a managed Claude launch composes --setting-sources");
    &argv[at + 1]
}

#[test]
fn a_declared_hire_launches_with_project_sources_and_an_undeclared_one_does_not() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::new();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let mut client: Client<
        io::BufReader<std::os::unix::net::UnixStream>,
        std::os::unix::net::UnixStream,
    > = Client::connect(settings.socket()).expect("connect to the daemon");
    let credential = || {
        OperatorCredential::new(
            std::fs::read_to_string(
                settings
                    .runtime_dir
                    .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
            )
            .expect("the daemon minted a credential"),
        )
        .expect("it is well formed")
    };

    let mut hire = |trusted: Option<bool>| {
        let mut params =
            serde_json::to_value(common::hire_params(fixture.state.path())).expect("params");
        if let Some(trusted) = trusted {
            params["trusted"] = serde_json::json!(trusted);
        }
        let response = client
            .call_as_operator(Method::AgentHire, Some(params), credential())
            .expect("agent.hire is answered");
        assert!(response.error.is_none(), "{:?}", response.error);
        let hired: HireResult =
            serde_json::from_value(response.result.expect("a result")).expect("a hire result");
        common::set_goal(&running.handle, common::goal_params(hired.agent, &["true"]));
        hired.agent
    };
    let declared = hire(Some(true));
    let undeclared = hire(None);

    let deadline = Instant::now() + Duration::from_secs(20);
    while host.spawned().len() < 2 {
        assert!(Instant::now() < deadline, "both turns launch");
        std::thread::sleep(Duration::from_millis(20));
    }
    running.stop(ExitCause::Ended);

    let recorded: Vec<bool> = fixture
        .records()
        .iter()
        .filter_map(|record| match record {
            Record::AgentHired { id, trusted, .. } if *id == declared || *id == undeclared => {
                Some(*trusted)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        recorded,
        [true, false],
        "the declaration is journaled once, and absence is untrusted"
    );

    let spawned = host.spawned();
    let sources = |at: usize| setting_sources(&spawned[at].argv).to_owned();
    assert_eq!(
        sources(0),
        OsString::from("project"),
        "a trusted hire launches with exactly the project setting source: {:?}",
        spawned[0].argv
    );
    assert_eq!(
        sources(1),
        OsString::new(),
        "an untrusted hire launches with an empty source list: {:?}",
        spawned[1].argv
    );
}

/// A provider whose adapter has no trusted mode refuses the declaration at `agent.hire`, before
/// the append, so the journal never records trust that nothing used. The Claude hire in the same
/// daemon is the positive control: it is admitted and recorded `trusted: true`.
#[test]
fn a_declaration_for_a_provider_without_a_trusted_mode_is_refused_and_journals_nothing() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let mut seams = seams(
        &settings,
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    seams.adapters = Adapters::new([
        Box::new(ClaudeAdapter::default()) as Box<dyn Adapter>,
        Box::new(CodexAdapter::default()),
        Box::new(PiAdapter::default()),
    ]);
    let daemon = boot(&settings, seams).expect("boot");
    let mut running = common::start(daemon);
    let mut client: Client<
        io::BufReader<std::os::unix::net::UnixStream>,
        std::os::unix::net::UnixStream,
    > = Client::connect(settings.socket()).expect("connect to the daemon");
    let credential = || {
        OperatorCredential::new(
            std::fs::read_to_string(
                settings
                    .runtime_dir
                    .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
            )
            .expect("the daemon minted a credential"),
        )
        .expect("it is well formed")
    };

    let mut hire = |provider: Provider| {
        let mut params =
            serde_json::to_value(common::hire_params(fixture.state.path())).expect("params");
        params["provider"] = serde_json::json!(provider);
        params["trusted"] = serde_json::json!(true);
        client
            .call_as_operator(Method::AgentHire, Some(params), credential())
            .expect("agent.hire is answered")
    };
    for provider in [Provider::Codex, Provider::Pi] {
        let error = hire(provider)
            .error
            .unwrap_or_else(|| panic!("a trusted {provider} hire is refused"));
        assert_eq!(error.code, ErrorCode::InvalidParams.code());
        assert_eq!(
            error.message,
            format!("agent.hire: {provider} does not support `trusted`")
        );
    }
    let claude = hire(Provider::Claude);
    assert!(claude.error.is_none(), "{:?}", claude.error);
    running.stop(ExitCause::Ended);

    let recorded: Vec<(Provider, bool)> = fixture
        .records()
        .iter()
        .filter_map(|record| match record {
            Record::AgentHired {
                provider, trusted, ..
            } => Some((*provider, *trusted)),
            _ => None,
        })
        .collect();
    assert_eq!(
        recorded,
        [(Provider::Claude, true)],
        "only the Claude hire reached the journal"
    );
}
