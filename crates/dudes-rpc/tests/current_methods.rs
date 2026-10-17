//! Current and legacy methods share dispatch, registration guards, and authorization.

use std::io::{self, Cursor, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::{CurrentMethod, ErrorCode, JSONRPC, Method, Request, RequestId, Response};
use dudes_rpc::auth::{PeerClassifier, PeerIdentity, PeerPid, PeerProvenance};
use dudes_rpc::server::serve_stream;
use dudes_rpc::testing::{AdmitAll, FakeMintedTokens, RefuseAll};
use dudes_rpc::{Dispatcher, Outbound, Requirement};
use dudes_types::AgentId;
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

fn credential(text: char) -> OperatorCredential {
    OperatorCredential::new(text.to_string().repeat(43)).unwrap()
}

fn operator_dispatcher(calls: Arc<AtomicUsize>) -> Dispatcher {
    let token = "t".repeat(43);
    let mut dispatcher = Dispatcher::new(PeerClassifier::new(
        credential('C'),
        Arc::new(FakeMintedTokens::minted(&[(&token, AgentId::NIL)])),
    ));
    dispatcher.register_current_operator(CurrentMethod::TicketCheckout, move |call, admission| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({
            "method": call.method.name(),
            "role": call.role.name(),
            "pid": admission.pid(),
            "uid": admission.uid(),
        }))
    });
    dispatcher.register(Method::SystemPing, Requirement::AnyRole, |call| {
        Ok(json!({"role": call.role.name()}))
    });
    dispatcher
}

#[derive(Clone, Default)]
struct Bytes(Arc<Mutex<Vec<u8>>>);

impl Write for Bytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serve_request(
    dispatcher: &Dispatcher,
    request: &Request,
    peer: PeerIdentity,
    provenance: PeerProvenance,
) -> Response {
    let bytes = Bytes::default();
    let outbound = Arc::new(Outbound::new(bytes.clone()));
    let mut frame = serde_json::to_vec(request).unwrap();
    frame.push(b'\n');
    serve_stream(Cursor::new(frame), &outbound, peer, provenance, dispatcher).unwrap();
    serde_json::from_slice(&bytes.0.lock().unwrap()).unwrap()
}

fn peer(pid: i32, uid: Option<u32>) -> PeerIdentity {
    PeerIdentity {
        uid,
        gid: None,
        pid: PeerPid::Reported {
            pid,
            pidversion: None,
        },
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
fn operator_registration_is_fixed_and_refuses_dedicated_routes() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = operator_dispatcher(Arc::clone(&calls));
    assert_eq!(
        dispatcher.current_requirement(CurrentMethod::TicketCheckout),
        Some(Requirement::Operator)
    );
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
            Dispatcher::new(AdmitAll)
                .register_current_operator(method.into(), |_, _| Ok(json!({})));
        }));
        assert!(
            rejected.is_err(),
            "{} bypassed its owned route",
            method.name()
        );
    }
}

#[test]
fn recovery_methods_refuse_generic_registration_routes() {
    for method in [
        CurrentMethod::RecoveryInspect,
        CurrentMethod::RecoverySubmit,
        CurrentMethod::RecoveryReceipt,
    ] {
        assert!(method.legacy().is_none());
        assert!(method.name().parse::<Method>().is_err());
        let audited = catch_unwind(AssertUnwindSafe(|| {
            Dispatcher::new(AdmitAll).register_current_audited(
                method,
                Requirement::AnyRole,
                |_, _| Ok(json!({})),
            );
        }));
        assert!(audited.is_err(), "{method} bypassed the typed route");
        let operator = catch_unwind(AssertUnwindSafe(|| {
            Dispatcher::new(AdmitAll).register_current_operator(method, |_, _| Ok(json!({})));
        }));
        assert!(operator.is_err(), "{method} bypassed the typed route");
    }
}

#[test]
fn operator_admission_uses_captured_pid_and_preserves_optional_uid() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = operator_dispatcher(Arc::clone(&calls));
    for (pid, uid) in [(4242, Some(501)), (4343, None)] {
        let mut request = request("ticket.checkout");
        request.cred = Some(credential('C'));
        request.params = Some(json!({"pid": 9999, "uid": 9999}));
        let response = serve_request(
            &dispatcher,
            &request,
            peer(pid, uid),
            PeerProvenance::NoRecordedAgent,
        );
        assert!(response.error.is_none());
        assert_eq!(
            response.result,
            Some(json!({"method": "ticket.checkout", "role": "operator", "pid": pid, "uid": uid}))
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn operator_admission_refuses_missing_zero_and_negative_transport_pid() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = operator_dispatcher(Arc::clone(&calls));
    let mut request = request("ticket.checkout");
    request.cred = Some(credential('C'));
    request.params = Some(json!({"pid": 4242, "uid": 501}));
    for peer in [
        PeerIdentity::unknown(),
        peer(0, Some(501)),
        peer(-1, Some(501)),
    ] {
        let response = serve_request(&dispatcher, &request, peer, PeerProvenance::NoRecordedAgent);
        assert!(response.result.is_none());
        let error = response.error.unwrap();
        assert_eq!(error.code, ErrorCode::Unauthorized.code());
        assert_eq!(error.message, "the call is not admitted");
        assert!(error.data.is_none());
    }
    let response = dispatcher.dispatch(
        &request,
        PeerProvenance::NoRecordedAgent,
        &Arc::new(Outbound::sink()),
    );
    assert_eq!(response.error.unwrap().code, ErrorCode::Unauthorized.code());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let response = serve_request(
        &dispatcher,
        &request,
        peer(4242, Some(501)),
        PeerProvenance::NoRecordedAgent,
    );
    assert_eq!(
        response.result,
        Some(json!({"method": "ticket.checkout", "role": "operator", "pid": 4242, "uid": 501}))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn operator_admission_refuses_denied_credentials_and_non_operator_roles() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = operator_dispatcher(Arc::clone(&calls));
    for (cred, params, provenance, resolved_role) in [
        (
            Some('X'),
            None,
            PeerProvenance::NoRecordedAgent,
            Some("unproven"),
        ),
        (
            None,
            None,
            PeerProvenance::NoRecordedAgent,
            Some("unproven"),
        ),
        (
            None,
            Some(json!({"token": "t".repeat(43)})),
            PeerProvenance::Agent(AgentId::NIL),
            Some("agent"),
        ),
        (Some('C'), None, PeerProvenance::Agent(AgentId::NIL), None),
    ] {
        let mut request = request("ticket.checkout");
        request.cred = cred.map(credential);
        request.params = params;
        let response = serve_request(&dispatcher, &request, peer(4242, Some(501)), provenance);
        assert!(response.result.is_none());
        let error = response.error.unwrap();
        assert_eq!(error.code, ErrorCode::Unauthorized.code());
        assert_eq!(
            error.message,
            if resolved_role.is_some() {
                "this method requires an operator"
            } else {
                "this peer may not call this method"
            }
        );
        assert!(error.data.is_none());
        request.method = "system.ping".to_owned();
        let probe = serve_request(&dispatcher, &request, peer(4242, Some(501)), provenance);
        if let Some(role) = resolved_role {
            assert_eq!(probe.result, Some(json!({"role": role})));
        } else {
            assert_eq!(probe.error.unwrap().code, ErrorCode::Unauthorized.code());
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let mut request = request("ticket.checkout");
    request.cred = Some(credential('C'));
    let response = serve_request(
        &dispatcher,
        &request,
        peer(4242, Some(501)),
        PeerProvenance::NoRecordedAgent,
    );
    assert_eq!(
        response.result,
        Some(json!({"method": "ticket.checkout", "role": "operator", "pid": 4242, "uid": 501}))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
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

    use std::os::unix::fs::MetadataExt;

    use dudes_rpc::testing::{FakeResolver, TestServer};

    #[test]
    fn operator_admission_reports_actual_unix_pid_and_uid_after_authorization() {
        let dir = tempfile::tempdir().unwrap();
        let uid = dir.path().metadata().unwrap().uid();
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = Arc::new(FakeResolver::new(PeerProvenance::NoRecordedAgent));
        let server = TestServer::start_with_resolver(
            dir.path().join("run"),
            operator_dispatcher(Arc::clone(&calls)),
            Arc::clone(&resolver) as Arc<dyn dudes_rpc::auth::PeerResolver>,
        );
        let mut client = server.client();
        let params = || Some(json!({"pid": 0, "uid": null}));
        let refused = client
            .call_current_as_operator(CurrentMethod::TicketCheckout, params(), credential('X'))
            .unwrap();
        assert_eq!(refused.error.unwrap().code, ErrorCode::Unauthorized.code());
        let absent = client
            .call_current_presenting(CurrentMethod::TicketCheckout, params(), None)
            .unwrap();
        assert_eq!(absent.error.unwrap().code, ErrorCode::Unauthorized.code());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let allowed = client
            .call_current_as_operator(CurrentMethod::TicketCheckout, params(), credential('C'))
            .unwrap();
        assert!(allowed.error.is_none());
        assert_eq!(
            allowed.result,
            Some(
                json!({"method": "ticket.checkout", "role": "operator", "pid": std::process::id(), "uid": uid})
            )
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let asked = resolver.asked();
        assert_eq!(asked.len(), 1);
        assert_eq!(
            asked[0].pid.reported(),
            Some(i32::try_from(std::process::id()).unwrap())
        );
        assert_eq!(asked[0].uid, Some(uid));
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
