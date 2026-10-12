//! Current and legacy methods share dispatch, registration guards, and authorization.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use dudes_proto::rpc::{CurrentMethod, ErrorCode, JSONRPC, Method, Request, RequestId};
use dudes_rpc::auth::PeerProvenance;
use dudes_rpc::testing::{AdmitAll, RefuseAll};
use dudes_rpc::{Dispatcher, Outbound, Requirement};
use serde_json::json;

fn request(method: &str) -> Request {
    Request {
        jsonrpc: JSONRPC.to_owned(),
        id: RequestId::String("current-call".to_owned()),
        method: method.to_owned(),
        params: None,
        v: None,
        cred: None,
    }
}

#[test]
fn current_and_legacy_registrations_dispatch_through_one_table() {
    let mut dispatcher = Dispatcher::new(AdmitAll);
    dispatcher.register_audited(Method::SystemDoctor, Requirement::AnyRole, |call, _| {
        Ok(json!({"method": call.method.name(), "legacy": true}))
    });
    dispatcher.register_current_audited(
        CurrentMethod::TicketCheckout,
        Requirement::AnyRole,
        |call, _| Ok(json!({"method": call.method.name(), "legacy": false})),
    );
    let outbound = Arc::new(Outbound::sink());
    for (method, expected) in [
        (
            "system.doctor",
            json!({"method": "system.doctor", "legacy": true}),
        ),
        (
            "ticket.checkout",
            json!({"method": "ticket.checkout", "legacy": false}),
        ),
    ] {
        let response =
            dispatcher.dispatch(&request(method), PeerProvenance::Unresolvable, &outbound);
        assert_eq!(response.id, RequestId::String("current-call".to_owned()));
        assert_eq!(response.result.unwrap(), expected);
    }
    assert_eq!(
        dispatcher.methods().collect::<Vec<_>>(),
        vec![Method::SystemDoctor]
    );
    assert_eq!(
        dispatcher.current_methods().collect::<Vec<_>>(),
        vec![
            CurrentMethod::Legacy(Method::SystemDoctor),
            CurrentMethod::TicketCheckout
        ]
    );
    assert_eq!(
        dispatcher.requirement(Method::SystemDoctor),
        Some(Requirement::AnyRole)
    );
    assert_eq!(
        dispatcher.current_requirement(CurrentMethod::TicketCheckout),
        Some(Requirement::AnyRole)
    );
}

#[test]
fn current_registration_refuses_conditioned_effects_and_bounded_questions() {
    for method in [
        Method::AgentSteer,
        Method::AgentAnswer,
        Method::AgentAttach,
        Method::AgentDetach,
        Method::PtyWrite,
        Method::MailSend,
        Method::MemoryAppend,
        Method::AgentAsk,
    ] {
        let rejected = catch_unwind(AssertUnwindSafe(|| {
            Dispatcher::new(AdmitAll).register_current_audited(
                method.into(),
                Requirement::OperatorOrAgent,
                |_, _| Ok(json!({})),
            );
        }));
        assert!(
            rejected.is_err(),
            "{} bypassed its owned route",
            method.name()
        );
    }
    let mut dispatcher = Dispatcher::new(AdmitAll);
    dispatcher.register_current_audited(
        CurrentMethod::TicketCheckout,
        Requirement::AnyRole,
        |_, _| Ok(json!({"checkout": true})),
    );
    assert_eq!(
        dispatcher
            .dispatch(
                &request("ticket.checkout"),
                PeerProvenance::Unresolvable,
                &Arc::new(Outbound::sink()),
            )
            .result
            .unwrap(),
        json!({"checkout": true})
    );
}

#[test]
fn unknown_current_names_remain_behind_authorization() {
    let outbound = Arc::new(Outbound::sink());
    for (dispatcher, expected) in [
        (Dispatcher::new(RefuseAll), ErrorCode::Unauthorized),
        (Dispatcher::new(AdmitAll), ErrorCode::MethodNotFound),
    ] {
        for method in ["ticket.checkout", "ticket.checkout.extra"] {
            let response =
                dispatcher.dispatch(&request(method), PeerProvenance::Unresolvable, &outbound);
            assert_eq!(response.error.unwrap().code, expected.code());
        }
    }
}

#[cfg(unix)]
mod socket {
    use super::*;

    use dudes_proto::rpc::cred::OperatorCredential;
    use dudes_rpc::auth::PeerClassifier;
    use dudes_rpc::testing::{FakeMintedTokens, FakeResolver, TestServer};

    fn credential(text: char) -> OperatorCredential {
        OperatorCredential::new(text.to_string().repeat(43)).unwrap()
    }

    #[test]
    fn current_operator_calls_use_the_shipped_classifier_and_report_the_peer_pid() {
        let dir = tempfile::tempdir().unwrap();
        let mut dispatcher = Dispatcher::new(PeerClassifier::new(
            credential('C'),
            Arc::new(FakeMintedTokens::none()),
        ));
        dispatcher.register_current_audited(
            CurrentMethod::TicketCheckout,
            Requirement::Operator,
            |call, pid| {
                Ok(json!({"method": call.method.name(), "role": call.role.name(), "pid": pid}))
            },
        );
        let server = TestServer::start_with_resolver(
            dir.path().join("run"),
            dispatcher,
            Arc::new(FakeResolver::new(PeerProvenance::NoRecordedAgent)),
        );
        let mut client = server.client();
        let refused = client
            .call_current_as_operator(CurrentMethod::TicketCheckout, None, credential('X'))
            .unwrap();
        assert_eq!(refused.error.unwrap().code, ErrorCode::Unauthorized.code());
        let absent = client
            .send_raw(r#"{"jsonrpc":"2.0","id":7,"method":"ticket.checkout"}"#)
            .unwrap();
        assert_eq!(absent.error.unwrap().code, ErrorCode::Unauthorized.code());
        let allowed = client
            .call_current_as_operator(CurrentMethod::TicketCheckout, None, credential('C'))
            .unwrap();
        assert_eq!(
            allowed.result.unwrap(),
            json!({"method": "ticket.checkout", "role": "operator", "pid": std::process::id()})
        );
    }
}
