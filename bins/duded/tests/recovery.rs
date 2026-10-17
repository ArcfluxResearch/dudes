//! The real recovery RPC port queues typed commands and uses a separate lookup after reply loss.

use std::io::{self, Cursor, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use duded::ports::{Answered, Ask, Handle, Job};
use dudes_proto::record::Hex32;
use dudes_proto::rpc::Response;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::recovery::{
    RecoveryKnownPrefix, RecoveryReceiptResult, RecoverySubmitRefusal, RecoverySubmitResult,
};
use dudes_rpc::{Dispatcher, Outbound, PeerClassifier, PeerIdentity, PeerPid, PeerProvenance};
use serde_json::json;

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

fn fixture() -> (Arc<Dispatcher>, mpsc::Receiver<Job>) {
    let (jobs, receiver) = mpsc::channel();
    let mut dispatcher = Dispatcher::new(PeerClassifier::new(
        OperatorCredential::new("C".repeat(43)).unwrap(),
        Arc::new(duded::classify::WorldTokens::new()),
    ));
    dudes_rpc::methods::register_recovery(&mut dispatcher, &Arc::new(Handle::new(jobs)));
    (Arc::new(dispatcher), receiver)
}

fn call(
    dispatcher: Arc<Dispatcher>,
    method: &'static str,
    params: serde_json::Value,
) -> thread::JoinHandle<Response> {
    thread::spawn(move || {
        let bytes = Bytes::default();
        let outbound = Arc::new(Outbound::new(bytes.clone()));
        let frame = format!(
            "{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":method,"params":params,"cred":"C".repeat(43)})
        );
        dudes_rpc::server::serve_stream(
            Cursor::new(frame.into_bytes()),
            &outbound,
            PeerIdentity {
                uid: Some(501),
                gid: None,
                pid: PeerPid::Reported {
                    pid: 4242,
                    pidversion: None,
                },
            },
            PeerProvenance::NoRecordedAgent,
            &dispatcher,
        )
        .unwrap();
        let response = serde_json::from_slice(&bytes.0.lock().unwrap()).unwrap();
        response
    })
}

fn params() -> serde_json::Value {
    json!({
        "request_id":"01".repeat(32),
        "expected": {
            "work": {"agent":"00000000010000000000000001","goal":"00000000010000000000000002","turn":"00000000010000000000000003"},
            "issue_lsn":5,"source_lsn":9,"episode":null,"expected_predecessor":null,"control_lsn":null,"ticket":null
        },
        "choice":"fresh_with_recap"
    })
}

#[test]
fn real_rpc_handle_adapter_preserves_explicit_selectors_and_transport_attribution() {
    let (dispatcher, jobs) = fixture();
    let response = call(dispatcher, "recovery.submit", params());
    let Job::Ask {
        ask: Ask::Recover { command, admission },
        reply,
    } = jobs.recv().unwrap()
    else {
        panic!("typed recovery command")
    };
    assert_eq!(command.request_id, Hex32::new([1; 32]));
    assert_eq!(command.expected.issue_lsn, dudes_types::Lsn(5));
    assert_eq!(command.expected.source_lsn, dudes_types::Lsn(9));
    assert_eq!(command.expected.predecessor, None);
    assert_eq!(command.expected.episode, None);
    assert_eq!(command.expected.control_lsn, None);
    assert_eq!(command.expected.ticket, None);
    assert_eq!((admission.pid(), admission.uid()), (4242, Some(501)));
    assert_eq!(
        command.attribution,
        dudes_proto::recovery::RecoveryAttribution::LocalOperator {
            pid: 4242,
            uid: Some(501)
        }
    );
    reply
        .send(Ok(Answered::RecoverySubmitted(
            RecoverySubmitResult::Refused {
                reason: RecoverySubmitRefusal::StaleSource,
                prefix: RecoveryKnownPrefix {
                    required: Some(dudes_types::Lsn(12)),
                    resume: None,
                },
            },
        )))
        .unwrap();
    let response = response.join().unwrap();
    assert!(response.error.is_none());
    assert_eq!(
        response.result,
        Some(
            json!({"state":"refused","reason":"stale_source","prefix":{"required":12,"resume":null}})
        )
    );
    assert!(jobs.try_recv().is_err());
}

#[test]
fn lost_submission_reply_returns_unknown_and_receipt_lookup_queues_no_resubmission() {
    let (dispatcher, jobs) = fixture();
    let response = call(Arc::clone(&dispatcher), "recovery.submit", params());
    let Job::Ask {
        ask: Ask::Recover { command, .. },
        reply,
    } = jobs.recv().unwrap()
    else {
        panic!("one mutation")
    };
    let request = command.request_id;
    drop(reply);
    let response = response.join().unwrap();
    assert_eq!(
        response.result,
        Some(json!({"state":"unknown","request_id":request}))
    );
    assert!(jobs.try_recv().is_err());
    let response = call(
        dispatcher,
        "recovery.receipt",
        json!({"request_id":request}),
    );
    let Job::Ask {
        ask: Ask::RecoveryReceipt {
            request: queried, ..
        },
        reply,
    } = jobs.recv().unwrap()
    else {
        panic!("lookup, without another mutation")
    };
    assert_eq!(queried, request);
    reply
        .send(Ok(Answered::RecoveryReceipt(
            RecoveryReceiptResult::Absent {
                checked_through: dudes_types::Lsn(31),
            },
        )))
        .unwrap();
    assert_eq!(
        response.join().unwrap().result,
        Some(json!({"state":"absent","checked_through":31}))
    );
    assert!(jobs.try_recv().is_err());
}
