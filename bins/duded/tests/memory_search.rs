//! Memory search through the production dispatcher and loop.
#![cfg(unix)]

mod common;

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use duded::startup::Witness;
use dudes_proto::rpc::{ErrorCode, Response};
use dudes_rpc::Client;
use serde_json::{Value, json};

type Socket = Client<BufReader<UnixStream>, UnixStream>;

fn connect(settings: &duded::Settings) -> Socket {
    let stream = UnixStream::connect(settings.socket()).expect("socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    Client::new(BufReader::new(stream.try_clone().unwrap()), stream)
}

fn search(settings: &duded::Settings, params: Value) -> Response {
    let credential = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("owned fixture credential");
    let mut request = json!({
        "jsonrpc":"2.0", "id":1, "method":"memory.search",
        "cred":credential,
    });
    request["params"] = params;
    connect(settings)
        .send_raw(&request.to_string())
        .expect("response")
}

fn daemon(fixture: &common::Fixture) -> (duded::Settings, common::Running, dudes_types::AgentId) {
    let settings = fixture.settings();
    let daemon = duded::boot(
        &settings,
        common::seams(
            &settings,
            common::SharedHost::new(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    (settings, running, agent)
}

#[test]
fn search_returns_an_empty_page_for_an_existing_agent_without_memory() {
    let fixture = common::Fixture::new();
    let (settings, mut running, agent) = daemon(&fixture);
    let response = search(&settings, json!({"agent":agent,"query":"remember"}));
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(
        response.result,
        Some(json!({"hits":[],"next_since_lsn":null}))
    );
    assert_eq!(
        running.stop(duded::ExitCause::ShutdownRpc),
        duded::ExitCause::ShutdownRpc
    );
}

#[test]
fn search_rejects_invalid_query_and_limit_before_returning_memory() {
    let fixture = common::Fixture::new();
    let (settings, mut running, agent) = daemon(&fixture);
    for params in [
        json!({"agent":agent,"query":"a\u{0}b"}),
        json!({"agent":agent,"query":""}),
        json!({"agent":agent,"query":"x".repeat(1025)}),
        json!({"agent":agent,"query":"remember","limit":0}),
        json!({"agent":agent,"query":"remember","limit":101}),
    ] {
        let response = search(&settings, params);
        assert_eq!(
            response.error.unwrap().code,
            ErrorCode::InvalidParams.code()
        );
        assert!(response.result.is_none());
    }
    assert_eq!(
        running.stop(duded::ExitCause::ShutdownRpc),
        duded::ExitCause::ShutdownRpc
    );
}

#[test]
fn search_requires_an_operator_target_and_reports_unknown_agents() {
    let fixture = common::Fixture::new();
    let (settings, mut running, _) = daemon(&fixture);
    assert_eq!(
        search(&settings, json!({"query":"remember"}))
            .error
            .unwrap()
            .code,
        ErrorCode::InvalidParams.code()
    );
    assert_eq!(
        search(
            &settings,
            json!({"agent":dudes_types::AgentId::NIL,"query":"remember"})
        )
        .error
        .unwrap()
        .code,
        ErrorCode::NotFound.code()
    );
    assert_eq!(
        running.stop(duded::ExitCause::ShutdownRpc),
        duded::ExitCause::ShutdownRpc
    );
}

#[test]
fn an_escaped_production_search_result_fits_a_frame_and_changes_no_records() {
    let fixture = common::Fixture::new();
    let (settings, mut running, agent) = daemon(&fixture);
    for _ in 0..25 {
        running
            .handle
            .ask(duded::ports::Ask::MemoryAppend {
                agent,
                payload: "\u{1}".repeat(1024).into_bytes(),
                provenance: dudes_proto::PeerProvenance::Agent(agent),
            })
            .unwrap();
    }
    let before = fixture.records();
    let response = search(
        &settings,
        json!({"agent":agent,"query":"\u{1}","limit":100}),
    );
    assert!(response.error.is_none());
    let result = response.result.clone().unwrap();
    assert!(serde_json::to_vec(&result).unwrap().len() <= 65_536);
    let page: dudes_proto::rpc::memory::MemorySearchResult =
        serde_json::from_value(result.clone()).unwrap();
    assert_eq!(page.hits.len(), 10);
    assert!(page.next_since_lsn.is_some());
    let mut frame = Vec::new();
    dudes_rpc::write_frame(&mut frame, &response).unwrap();
    assert!(frame.len() < dudes_rpc::MAX_FRAME_BYTES);
    let oversized = Response::ok(
        dudes_proto::rpc::RequestId::String("i".repeat(dudes_rpc::MAX_FRAME_BYTES)),
        result,
    );
    let mut refused = Vec::new();
    assert!(dudes_rpc::write_frame(&mut refused, &oversized).is_err());
    assert!(refused.is_empty());
    assert_eq!(fixture.records(), before);
    assert!(before.iter().all(|record| !matches!(
        record,
        dudes_proto::Record::LoopDecided {
            decision: dudes_proto::LoopDecision::Done,
            ..
        }
    )));
    assert_eq!(
        running.stop(duded::ExitCause::ShutdownRpc),
        duded::ExitCause::ShutdownRpc
    );
}
