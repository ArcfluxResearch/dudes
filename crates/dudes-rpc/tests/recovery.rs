//! Recovery RPC admission and original-frame contract checks.

use std::io::{self, Cursor, Write};
use std::sync::{Arc, Mutex};

use dudes_proto::record::Hex32;
use dudes_proto::recovery::{RecoveryClaimRef, RecoveryEpisodeId, RecoveryWork};
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::recovery::{
    ExpectedRecovery, RecoveryChoice, RecoveryInspectParams, RecoveryInspectResult,
    RecoveryInspectUnavailable, RecoveryKnownPrefix, RecoveryReceiptParams, RecoveryReceiptResult,
    RecoverySubmitParams, RecoverySubmitRefusal, RecoverySubmitResult, ReviewedTicketRound,
};
use dudes_proto::rpc::{CurrentMethod, ErrorCode, Method, Request, Response, RpcError};
use dudes_rpc::dispatch::OperatorAdmission;
use dudes_rpc::methods::{RecoveryControl, register_recovery};
use dudes_rpc::server::serve_stream;
use dudes_rpc::testing::FakeMintedTokens;
use dudes_rpc::{Dispatcher, Outbound, PeerClassifier, PeerIdentity, PeerPid, PeerProvenance};
use dudes_types::{AgentId, CardId, GoalId, Lsn, TurnId};
use serde_json::json;

const AGENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const INSPECT: &str = r#"{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}"#;
const RECEIPT: &str =
    r#"{"request_id":"0000000000000000000000000000000000000000000000000000000000000000"}"#;
const SUBMIT: &str = r#"{"request_id":"0000000000000000000000000000000000000000000000000000000000000000","expected":{"work":{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","goal":"01ARZ3NDEKTSV4RRFFQ69G5FAW","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX"},"issue_lsn":20,"source_lsn":80,"episode":{"work":{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","goal":"01ARZ3NDEKTSV4RRFFQ69G5FAW","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX"},"origin":90},"expected_predecessor":{"episode":{"work":{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","goal":"01ARZ3NDEKTSV4RRFFQ69G5FAW","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX"},"origin":90},"authorization_lsn":300},"control_lsn":90,"ticket":{"card":"01ARZ3NDEKTSV4RRFFQ69G5FAY","round":2}},"choice":"repeat_issued_fresh"}"#;

#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Inspect(RecoveryInspectParams, u32, Option<u32>),
    Submit(Box<RecoverySubmitParams>, u32, Option<u32>),
    Receipt(RecoveryReceiptParams, u32, Option<u32>),
}

#[derive(Default)]
struct Control(Mutex<Vec<Invocation>>);

impl RecoveryControl for Control {
    fn inspect_recovery(
        &self,
        params: RecoveryInspectParams,
        admission: OperatorAdmission,
    ) -> Result<RecoveryInspectResult, RpcError> {
        self.0.lock().unwrap().push(Invocation::Inspect(
            params,
            admission.pid(),
            admission.uid(),
        ));
        Ok(RecoveryInspectResult::Unavailable {
            reason: RecoveryInspectUnavailable::NotRecoverable,
        })
    }

    fn submit_recovery(
        &self,
        params: &RecoverySubmitParams,
        admission: OperatorAdmission,
    ) -> Result<RecoverySubmitResult, RpcError> {
        self.0.lock().unwrap().push(Invocation::Submit(
            Box::new(*params),
            admission.pid(),
            admission.uid(),
        ));
        Ok(RecoverySubmitResult::Refused {
            reason: RecoverySubmitRefusal::ChoiceUnavailable,
            prefix: RecoveryKnownPrefix {
                required: Some(Lsn(41)),
                resume: None,
            },
        })
    }

    fn recovery_receipt(
        &self,
        params: RecoveryReceiptParams,
        admission: OperatorAdmission,
    ) -> Result<RecoveryReceiptResult, RpcError> {
        self.0.lock().unwrap().push(Invocation::Receipt(
            params,
            admission.pid(),
            admission.uid(),
        ));
        Ok(RecoveryReceiptResult::Absent {
            checked_through: Lsn(87),
        })
    }
}

fn fixture() -> (Dispatcher, Arc<Control>) {
    let control = Arc::new(Control::default());
    let token = "t".repeat(43);
    let mut dispatcher = Dispatcher::new(PeerClassifier::new(
        OperatorCredential::new("C".repeat(43)).unwrap(),
        Arc::new(FakeMintedTokens::minted(&[(
            &token,
            AgentId::parse(AGENT).unwrap(),
        )])),
    ));
    register_recovery(&mut dispatcher, &control);
    (dispatcher, control)
}

fn frame(method: CurrentMethod, params: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":"recovery-call","method":"{method}","params":{params},"cred":"{}"}}"#,
        "C".repeat(43)
    )
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

fn serve(
    dispatcher: &Dispatcher,
    line: &str,
    peer: PeerIdentity,
    provenance: PeerProvenance,
    delimiter: &str,
) -> Response {
    let bytes = Bytes::default();
    let outbound = Arc::new(Outbound::new(bytes.clone()));
    serve_stream(
        Cursor::new(format!("{line}{delimiter}").into_bytes()),
        &outbound,
        peer,
        provenance,
        dispatcher,
    )
    .unwrap();
    serde_json::from_slice(&bytes.0.lock().unwrap()).unwrap()
}

fn admitted(dispatcher: &Dispatcher, line: &str) -> Response {
    serve(
        dispatcher,
        line,
        peer(4242, Some(501)),
        PeerProvenance::NoRecordedAgent,
        "\n",
    )
}

fn expected_submit() -> RecoverySubmitParams {
    let work = RecoveryWork {
        agent: AgentId::parse(AGENT).unwrap(),
        goal: GoalId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap(),
        turn: TurnId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAX").unwrap(),
    };
    let episode = RecoveryEpisodeId {
        work,
        origin: Lsn(90),
    };
    RecoverySubmitParams {
        request_id: Hex32::new([0; 32]),
        expected: ExpectedRecovery {
            work,
            issue_lsn: Lsn(20),
            source_lsn: Lsn(80),
            episode: Some(episode),
            expected_predecessor: Some(RecoveryClaimRef {
                episode,
                authorization_lsn: Lsn(300),
            }),
            control_lsn: Some(Lsn(90)),
            ticket: Some(ReviewedTicketRound {
                card: CardId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAY").unwrap(),
                round: 2,
            }),
        },
        choice: RecoveryChoice::RepeatIssuedFresh,
    }
}

#[test]
fn recovery_routes_pass_exact_typed_inputs_and_bounded_results_after_operator_admission() {
    let (dispatcher, control) = fixture();
    for (method, params, result) in [
        (
            CurrentMethod::RecoveryInspect,
            INSPECT,
            json!({"state":"unavailable","reason":"not_recoverable"}),
        ),
        (
            CurrentMethod::RecoverySubmit,
            SUBMIT,
            json!({"state":"refused","reason":"choice_unavailable","prefix":{"required":41,"resume":null}}),
        ),
        (
            CurrentMethod::RecoveryReceipt,
            RECEIPT,
            json!({"state":"absent","checked_through":87}),
        ),
    ] {
        assert_eq!(
            dispatcher.current_requirement(method),
            Some(dudes_rpc::Requirement::Operator)
        );
        let response = admitted(&dispatcher, &frame(method, params));
        assert!(response.error.is_none());
        assert_eq!(response.result, Some(result));
    }
    assert_eq!(
        *control.0.lock().unwrap(),
        [
            Invocation::Inspect(
                RecoveryInspectParams {
                    agent: AgentId::parse(AGENT).unwrap()
                },
                4242,
                Some(501)
            ),
            Invocation::Submit(Box::new(expected_submit()), 4242, Some(501)),
            Invocation::Receipt(
                RecoveryReceiptParams {
                    request_id: Hex32::new([0; 32])
                },
                4242,
                Some(501)
            ),
        ]
    );
}

#[test]
fn original_recovery_authority_rejects_unknown_duplicate_and_escaped_members_recursively() {
    let (dispatcher, control) = fixture();
    for (prefix, key, escaped, value) in [
        (
            "{",
            "request_id",
            r"request_\u0069d",
            r#""0000000000000000000000000000000000000000000000000000000000000000""#,
        ),
        (r#""expected":{"#, "issue_lsn", r"issue_\u006csn", "20"),
        (
            r#""work":{"#,
            "agent",
            r"a\u0067ent",
            r#""01ARZ3NDEKTSV4RRFFQ69G5FAV""#,
        ),
        (r#""episode":{"#, "origin", r"ori\u0067in", "90"),
        (
            r#""episode":{"work":{"#,
            "agent",
            r"a\u0067ent",
            r#""01ARZ3NDEKTSV4RRFFQ69G5FAV""#,
        ),
        (
            r#""expected_predecessor":{"#,
            "authorization_lsn",
            r"authorization_\u006csn",
            "300",
        ),
        (
            r#""expected_predecessor":{"episode":{"#,
            "origin",
            r"ori\u0067in",
            "90",
        ),
        (
            r#""expected_predecessor":{"episode":{"work":{"#,
            "agent",
            r"a\u0067ent",
            r#""01ARZ3NDEKTSV4RRFFQ69G5FAV""#,
        ),
        (r#""ticket":{"#, "round", r"ro\u0075nd", "2"),
    ] {
        for member in [
            r#""extra":0"#.to_owned(),
            format!(r#""{key}":{value}"#),
            format!(r#""{escaped}":{value}"#),
        ] {
            let changed = SUBMIT.replacen(prefix, &format!("{prefix}{member},"), 1);
            assert_ne!(changed, SUBMIT);
            let response = admitted(&dispatcher, &frame(CurrentMethod::RecoverySubmit, &changed));
            assert!(response.result.is_none());
            assert_eq!(
                response.error.unwrap().code,
                ErrorCode::InvalidParams.code(),
                "{prefix} {member}"
            );
        }
    }
    assert!(control.0.lock().unwrap().is_empty());
    assert!(
        admitted(&dispatcher, &frame(CurrentMethod::RecoverySubmit, SUBMIT))
            .error
            .is_none()
    );
    assert_eq!(
        *control.0.lock().unwrap(),
        [Invocation::Submit(
            Box::new(expected_submit()),
            4242,
            Some(501)
        )]
    );
}

#[test]
fn inspection_and_receipt_reject_duplicate_escaped_unknown_and_array_params() {
    let (dispatcher, control) = fixture();
    for (method, params, key, escaped, value) in [
        (
            CurrentMethod::RecoveryInspect,
            INSPECT,
            "agent",
            r"a\u0067ent",
            r#""01ARZ3NDEKTSV4RRFFQ69G5FAV""#,
        ),
        (
            CurrentMethod::RecoveryReceipt,
            RECEIPT,
            "request_id",
            r"request_\u0069d",
            r#""0000000000000000000000000000000000000000000000000000000000000000""#,
        ),
    ] {
        for member in [
            r#""pid":9999"#.to_owned(),
            r#""uid":9999"#.to_owned(),
            format!(r#""{key}":{value}"#),
            format!(r#""{escaped}":{value}"#),
        ] {
            let changed = params.replacen('{', &format!("{{{member},"), 1);
            let response = admitted(&dispatcher, &frame(method, &changed));
            assert!(response.result.is_none());
            assert_eq!(
                response.error.unwrap().code,
                ErrorCode::InvalidParams.code()
            );
        }
        let response = admitted(&dispatcher, &frame(method, &format!("[{value}]")));
        assert_eq!(
            response.error.unwrap().code,
            ErrorCode::InvalidParams.code()
        );
    }
    assert!(control.0.lock().unwrap().is_empty());
    assert!(
        admitted(&dispatcher, &frame(CurrentMethod::RecoveryInspect, INSPECT))
            .error
            .is_none()
    );
    assert!(
        admitted(&dispatcher, &frame(CurrentMethod::RecoveryReceipt, RECEIPT))
            .error
            .is_none()
    );
    assert_eq!(
        *control.0.lock().unwrap(),
        [
            Invocation::Inspect(
                RecoveryInspectParams {
                    agent: AgentId::parse(AGENT).unwrap()
                },
                4242,
                Some(501)
            ),
            Invocation::Receipt(
                RecoveryReceiptParams {
                    request_id: Hex32::new([0; 32])
                },
                4242,
                Some(501)
            ),
        ]
    );
}

#[test]
fn recovery_envelope_refuses_ambiguous_methods_unknown_fields_and_non_objects() {
    let (dispatcher, control) = fixture();
    let line = frame(CurrentMethod::RecoveryInspect, INSPECT);
    for changed in [
        line.replacen('{', r#"{"extra":0,"#, 1),
        line.replacen('{', r#"{"id":2,"#, 1),
        line.replacen('{', r#"{"\u0069d":2,"#, 1),
        line.replacen('{', r#"{"method":"system.ping","#, 1),
        line.replace(
            r#""method":"recovery.inspect""#,
            r#""method":"recovery.inspect","method":"system.ping""#,
        ),
        line.replace(
            r#""method":"recovery.inspect""#,
            r#""method":"recovery.inspect","metho\u0064":"system.ping""#,
        ),
        format!(
            r#"["2.0","recovery-call","recovery.inspect",{INSPECT},null,"{}"]"#,
            "C".repeat(43)
        ),
    ] {
        let response = admitted(&dispatcher, &changed);
        assert!(response.result.is_none());
        assert_eq!(
            response.error.unwrap().code,
            ErrorCode::InvalidRequest.code(),
            "{changed}"
        );
    }
    assert!(control.0.lock().unwrap().is_empty());
    assert_eq!(
        admitted(&dispatcher, &line).result,
        Some(json!({"state":"unavailable","reason":"not_recoverable"}))
    );
    assert_eq!(control.0.lock().unwrap().len(), 1);
}

#[test]
fn recovery_json_text_bound_counts_whitespace_and_escapes_before_lf_or_crlf() {
    let (dispatcher, control) = fixture();
    for delimiter in ["\n", "\r\n"] {
        for (method, params) in [
            (CurrentMethod::RecoveryInspect, INSPECT),
            (CurrentMethod::RecoverySubmit, SUBMIT),
            (CurrentMethod::RecoveryReceipt, RECEIPT),
        ] {
            let mut line = frame(method, params).replace("recovery.", r"recovery\u002e");
            line.extend(std::iter::repeat_n(' ', 4096 - line.len()));
            assert_eq!(line.len(), 4096);
            let response = serve(
                &dispatcher,
                &line,
                peer(4242, Some(501)),
                PeerProvenance::NoRecordedAgent,
                delimiter,
            );
            assert!(response.error.is_none(), "{method} {delimiter:?}");
            let count = control.0.lock().unwrap().len();
            line.push(' ');
            assert_eq!(line.len(), 4097);
            let response = serve(
                &dispatcher,
                &line,
                peer(4242, Some(501)),
                PeerProvenance::NoRecordedAgent,
                delimiter,
            );
            let error = response.error.unwrap();
            assert_eq!(error.code, ErrorCode::InvalidParams.code());
            assert_eq!(error.message, "recovery frame exceeds 4096 bytes");
            assert_eq!(control.0.lock().unwrap().len(), count);
        }
    }
    assert_eq!(control.0.lock().unwrap().len(), 6);
}

#[test]
fn value_only_recovery_dispatch_cannot_reach_any_port() {
    let (dispatcher, control) = fixture();
    for (method, params) in [
        (CurrentMethod::RecoveryInspect, INSPECT),
        (CurrentMethod::RecoverySubmit, SUBMIT),
        (CurrentMethod::RecoveryReceipt, RECEIPT),
    ] {
        let line = frame(method, params);
        let request: Request = serde_json::from_str(&line).unwrap();
        let response = dispatcher.dispatch(
            &request,
            PeerProvenance::NoRecordedAgent,
            &Arc::new(Outbound::sink()),
        );
        let error = response.error.unwrap();
        assert_eq!(error.code, ErrorCode::InvalidParams.code());
        assert_eq!(error.message, "recovery requires original-frame validation");
    }
    assert!(control.0.lock().unwrap().is_empty());
    assert!(
        admitted(&dispatcher, &frame(CurrentMethod::RecoverySubmit, SUBMIT))
            .error
            .is_none()
    );
    assert_eq!(
        *control.0.lock().unwrap(),
        [Invocation::Submit(
            Box::new(expected_submit()),
            4242,
            Some(501)
        )]
    );
}

#[test]
fn recovery_requires_explicit_nulls_and_the_existing_ticket_round_range() {
    let (dispatcher, control) = fixture();
    let original: serde_json::Value = serde_json::from_str(SUBMIT).unwrap();
    let mut nullable = original.clone();
    for key in ["episode", "expected_predecessor", "control_lsn", "ticket"] {
        nullable["expected"][key] = serde_json::Value::Null;
        let mut omitted = original.clone();
        omitted["expected"].as_object_mut().unwrap().remove(key);
        let response = admitted(
            &dispatcher,
            &frame(CurrentMethod::RecoverySubmit, &omitted.to_string()),
        );
        assert_eq!(
            response.error.unwrap().code,
            ErrorCode::InvalidParams.code(),
            "{key}"
        );
    }
    for (pointer, value) in [
        ("/expected/ticket/round", json!(-1)),
        ("/expected/ticket/round", json!(4_294_967_296_u64)),
        ("/request_id", json!("A".repeat(64))),
        ("/choice", json!("fresh")),
        (
            "/choice",
            json!({"fresh_with_recap":{"recap":"caller text"}}),
        ),
    ] {
        let mut changed = original.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        let response = admitted(
            &dispatcher,
            &frame(CurrentMethod::RecoverySubmit, &changed.to_string()),
        );
        assert_eq!(
            response.error.unwrap().code,
            ErrorCode::InvalidParams.code(),
            "{pointer}"
        );
    }
    assert!(control.0.lock().unwrap().is_empty());
    assert!(
        admitted(
            &dispatcher,
            &frame(CurrentMethod::RecoverySubmit, &nullable.to_string())
        )
        .error
        .is_none()
    );
    let mut expected = expected_submit();
    expected.expected.episode = None;
    expected.expected.expected_predecessor = None;
    expected.expected.control_lsn = None;
    expected.expected.ticket = None;
    assert_eq!(
        *control.0.lock().unwrap(),
        [Invocation::Submit(Box::new(expected), 4242, Some(501))]
    );
    for round in [0, 4_294_967_295_u32] {
        let mut changed = original.clone();
        changed["expected"]["ticket"]["round"] = json!(round);
        assert!(
            admitted(
                &dispatcher,
                &frame(CurrentMethod::RecoverySubmit, &changed.to_string())
            )
            .error
            .is_none()
        );
        let mut expected = expected_submit();
        expected.expected.ticket.as_mut().unwrap().round = round;
        assert_eq!(
            control.0.lock().unwrap().last(),
            Some(&Invocation::Submit(Box::new(expected), 4242, Some(501)))
        );
    }
}

#[test]
fn recovery_rejects_array_shapes_at_each_authority_object() {
    let (dispatcher, control) = fixture();
    let original: serde_json::Value = serde_json::from_str(SUBMIT).unwrap();
    for (pointer, fields) in [
        ("", &["request_id", "expected", "choice"][..]),
        (
            "/expected",
            &[
                "work",
                "issue_lsn",
                "source_lsn",
                "episode",
                "expected_predecessor",
                "control_lsn",
                "ticket",
            ][..],
        ),
        ("/expected/work", &["agent", "goal", "turn"][..]),
        ("/expected/episode", &["work", "origin"][..]),
        ("/expected/episode/work", &["agent", "goal", "turn"][..]),
        (
            "/expected/expected_predecessor",
            &["episode", "authorization_lsn"][..],
        ),
        (
            "/expected/expected_predecessor/episode",
            &["work", "origin"][..],
        ),
        (
            "/expected/expected_predecessor/episode/work",
            &["agent", "goal", "turn"][..],
        ),
        ("/expected/ticket", &["card", "round"][..]),
    ] {
        let mut array = original.clone();
        let object = array.pointer_mut(pointer).unwrap();
        *object =
            serde_json::Value::Array(fields.iter().map(|field| object[*field].clone()).collect());
        let response = admitted(
            &dispatcher,
            &frame(CurrentMethod::RecoverySubmit, &array.to_string()),
        );
        assert_eq!(
            response.error.unwrap().code,
            ErrorCode::InvalidParams.code(),
            "{pointer}"
        );
    }
    assert!(control.0.lock().unwrap().is_empty());
    assert!(
        admitted(&dispatcher, &frame(CurrentMethod::RecoverySubmit, SUBMIT))
            .error
            .is_none()
    );
    assert_eq!(
        *control.0.lock().unwrap(),
        [Invocation::Submit(
            Box::new(expected_submit()),
            4242,
            Some(501)
        )]
    );
}

#[test]
fn all_recovery_methods_refuse_malformed_queries_and_unsupported_envelope_versions() {
    let (mut dispatcher, control) = fixture();
    for (method, params) in [
        (CurrentMethod::RecoveryInspect, INSPECT),
        (CurrentMethod::RecoverySubmit, SUBMIT),
        (CurrentMethod::RecoveryReceipt, RECEIPT),
    ] {
        let line = frame(method, params);
        for changed in [
            line.replacen('{', r#"{"v":2,"#, 1),
            line.replacen('{', r#"{"v":"1","#, 1),
            line.replacen('{', r#"{"v":1,"v":1,"#, 1),
            line.replacen('{', r#"{"jsonrpc":"1.0","#, 1),
            line.replace(r#""jsonrpc":"2.0""#, r#""jsonrpc":"1.0""#),
        ] {
            assert_eq!(
                admitted(&dispatcher, &changed).error.unwrap().code,
                ErrorCode::InvalidRequest.code()
            );
        }
        for bad in ["null", "[]", "{}", r#"{"extra":0}"#] {
            assert_eq!(
                admitted(&dispatcher, &frame(method, bad))
                    .error
                    .unwrap()
                    .code,
                ErrorCode::InvalidParams.code()
            );
        }
        let missing = line.replace(&format!(r#","params":{params}"#), "");
        assert_ne!(missing, line);
        assert_eq!(
            admitted(&dispatcher, &missing).error.unwrap().code,
            ErrorCode::InvalidParams.code()
        );
    }
    assert!(control.0.lock().unwrap().is_empty());
    for (method, params) in [
        (CurrentMethod::RecoveryInspect, INSPECT),
        (CurrentMethod::RecoverySubmit, SUBMIT),
        (CurrentMethod::RecoveryReceipt, RECEIPT),
    ] {
        let line = frame(method, params);
        for changed in [
            line.clone(),
            line.replacen('{', r#"{"v":1,"#, 1),
            line.replacen('{', r#"{"v":null,"#, 1),
        ] {
            assert!(admitted(&dispatcher, &changed).error.is_none());
        }
    }
    assert_eq!(control.0.lock().unwrap().len(), 9);
    dispatcher.register(Method::SystemPing, dudes_rpc::Requirement::AnyRole, |_| {
        Ok(json!({"legacy":true}))
    });
    assert_eq!(
        admitted(
            &dispatcher,
            r#"{"jsonrpc":"2.0","id":1,"method":"system.ping","v":2}"#
        )
        .result,
        Some(json!({"legacy":true}))
    );
}

#[test]
fn recovery_ports_refuse_non_operator_and_pidless_calls_and_keep_uid_absence() {
    let (dispatcher, control) = fixture();
    for (method, params) in [
        (CurrentMethod::RecoveryInspect, INSPECT),
        (CurrentMethod::RecoverySubmit, SUBMIT),
        (CurrentMethod::RecoveryReceipt, RECEIPT),
    ] {
        let line = frame(method, params);
        for peer in [
            PeerIdentity::unknown(),
            peer(0, Some(501)),
            peer(-1, Some(501)),
        ] {
            let response = serve(
                &dispatcher,
                &line,
                peer,
                PeerProvenance::NoRecordedAgent,
                "\n",
            );
            assert!(response.result.is_none());
            assert_eq!(response.error.unwrap().code, ErrorCode::Unauthorized.code());
        }
        for changed in [
            line.replace(&"C".repeat(43), &"X".repeat(43)),
            line.replace(&format!(r#","cred":"{}""#, "C".repeat(43)), ""),
        ] {
            assert_ne!(changed, line);
            let response = admitted(&dispatcher, &changed);
            assert!(response.result.is_none());
            assert_eq!(response.error.unwrap().code, ErrorCode::Unauthorized.code());
        }
        let response = serve(
            &dispatcher,
            &line,
            peer(4242, Some(501)),
            PeerProvenance::Agent(AgentId::parse(AGENT).unwrap()),
            "\n",
        );
        assert_eq!(response.error.unwrap().code, ErrorCode::Unauthorized.code());
    }
    assert!(control.0.lock().unwrap().is_empty());
    for (method, params) in [
        (CurrentMethod::RecoveryInspect, INSPECT),
        (CurrentMethod::RecoverySubmit, SUBMIT),
        (CurrentMethod::RecoveryReceipt, RECEIPT),
    ] {
        assert!(
            serve(
                &dispatcher,
                &frame(method, params),
                peer(4343, None),
                PeerProvenance::NoRecordedAgent,
                "\r\n"
            )
            .error
            .is_none()
        );
    }
    assert_eq!(
        *control.0.lock().unwrap(),
        [
            Invocation::Inspect(
                RecoveryInspectParams {
                    agent: AgentId::parse(AGENT).unwrap()
                },
                4343,
                None
            ),
            Invocation::Submit(Box::new(expected_submit()), 4343, None),
            Invocation::Receipt(
                RecoveryReceiptParams {
                    request_id: Hex32::new([0; 32])
                },
                4343,
                None
            ),
        ]
    );
}

#[cfg(unix)]
#[test]
fn real_unix_recovery_routes_use_the_shipped_classifier_and_captured_peer() {
    use dudes_rpc::testing::{FakeResolver, TestServer};
    use std::os::unix::fs::MetadataExt;

    let (dispatcher, control) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let uid = dir.path().metadata().unwrap().uid();
    let resolver = Arc::new(FakeResolver::new(PeerProvenance::NoRecordedAgent));
    let server = TestServer::start_with_resolver(
        dir.path().join("run"),
        dispatcher,
        Arc::clone(&resolver) as Arc<dyn dudes_rpc::PeerResolver>,
    );
    let mut client = server.client();
    let line = frame(CurrentMethod::RecoveryInspect, INSPECT);
    let refused = client
        .send_raw(&line.replace(&"C".repeat(43), &"X".repeat(43)))
        .unwrap();
    assert_eq!(refused.error.unwrap().code, ErrorCode::Unauthorized.code());
    assert!(control.0.lock().unwrap().is_empty());
    for (method, params) in [
        (CurrentMethod::RecoveryInspect, INSPECT),
        (CurrentMethod::RecoverySubmit, SUBMIT),
        (CurrentMethod::RecoveryReceipt, RECEIPT),
    ] {
        assert!(
            client
                .send_raw(&frame(method, params))
                .unwrap()
                .error
                .is_none()
        );
    }
    let pid = std::process::id();
    assert_eq!(
        *control.0.lock().unwrap(),
        [
            Invocation::Inspect(
                RecoveryInspectParams {
                    agent: AgentId::parse(AGENT).unwrap()
                },
                pid,
                Some(uid)
            ),
            Invocation::Submit(Box::new(expected_submit()), pid, Some(uid)),
            Invocation::Receipt(
                RecoveryReceiptParams {
                    request_id: Hex32::new([0; 32])
                },
                pid,
                Some(uid)
            ),
        ]
    );
    let asked = resolver.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].pid.reported(), Some(i32::try_from(pid).unwrap()));
    assert_eq!(asked[0].uid, Some(uid));
}
