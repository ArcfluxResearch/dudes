use super::{ActionHead, FloorSessions, Handle, Response, error_response, json_response};
use crate::floor_recovery::{
    decode_inspect, decode_receipt, decode_submit, encode_inspection, encode_receipt,
    encode_submission,
};
use crate::ports::FloorRecoveryReadError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RecoveryRoute {
    Inspect,
    Submit,
    Receipt,
}

pub(super) fn response<'a>(
    route: RecoveryRoute,
    sessions: &FloorSessions,
    handle: &Handle,
    head: &ActionHead,
    body: &[u8],
) -> Response<'a> {
    let mut code = 200;
    let encoded = if route == RecoveryRoute::Receipt {
        let Some(reader) =
            sessions.authorize_receipt(&head.host, &head.origin, &head.cookie, &head.csrf)
        else {
            return error_response(403, "Forbidden");
        };
        let Ok(request) = decode_receipt(body) else {
            return error_response(400, "Bad Request");
        };
        match handle.floor_recovery_receipt(reader, request.request_id) {
            Ok(result) => encode_receipt(&result),
            Err(error) => return read_error(error),
        }
    } else {
        let Some(session) =
            sessions.authorize_onboard(&head.host, &head.origin, &head.cookie, &head.csrf)
        else {
            return error_response(403, "Forbidden");
        };
        match route {
            RecoveryRoute::Inspect => {
                let Ok(request) = decode_inspect(body) else {
                    return error_response(400, "Bad Request");
                };
                match handle.inspect_floor_recovery(session, request.agent) {
                    Ok(result) => encode_inspection(&result),
                    Err(error) => return read_error(error),
                }
            }
            RecoveryRoute::Submit => {
                let Ok(intent) = decode_submit(body) else {
                    return error_response(400, "Bad Request");
                };
                let result = handle.submit_floor_recovery(session, &intent);
                code = match &result {
                    dudes_proto::rpc::recovery::RecoverySubmitResult::Accepted { .. } => 200,
                    dudes_proto::rpc::recovery::RecoverySubmitResult::Refused { .. } => 409,
                    dudes_proto::rpc::recovery::RecoverySubmitResult::Unknown { .. } => 202,
                };
                encode_submission(&result)
            }
            RecoveryRoute::Receipt => unreachable!("receipt uses nonrefreshing authentication"),
        }
    };
    match encoded {
        Ok(body) => json_response(
            code,
            match code {
                202 => "Accepted",
                409 => "Conflict",
                _ => "OK",
            },
            body,
        ),
        Err(_) => error_response(503, "Service Unavailable"),
    }
}

fn read_error(error: FloorRecoveryReadError) -> Response<'static> {
    match error {
        FloorRecoveryReadError::Forbidden => error_response(403, "Forbidden"),
        FloorRecoveryReadError::Unavailable => error_response(503, "Service Unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::Job;
    use std::sync::{Arc, mpsc};

    #[test]
    fn a_lost_submit_reply_returns_the_same_request_as_unknown_once() {
        let sessions = FloorSessions::new(
            "127.0.0.1:43123".parse().unwrap(),
            Arc::new(dudes_supervisor::testing::FakeClock::frozen(0)),
        );
        let exchange = sessions
            .exchange(
                "127.0.0.1:43123",
                "http://127.0.0.1:43123",
                sessions.issue().unwrap().bootstrap.as_bytes(),
            )
            .unwrap();
        let head = ActionHead {
            content_length: 0,
            host: "127.0.0.1:43123".into(),
            origin: "http://127.0.0.1:43123".into(),
            cookie: exchange.cookie,
            csrf: exchange.csrf,
        };
        let body = br#"{"request_id":"8484848484848484848484848484848484848484848484848484848484848484","expected":{"work":{"agent":"01ARZ3NDEKTSV4RRFFQ69G5FAV","goal":"01ARZ3NDEKTSV4RRFFQ69G5FAW","turn":"01ARZ3NDEKTSV4RRFFQ69G5FAX"},"issue_lsn":"9007199254740993","source_lsn":"18446744073709551615","episode":null,"expected_predecessor":null,"control_lsn":null,"ticket":null},"choice":"fresh_with_recap"}"#;
        let (sender, jobs) = mpsc::channel();
        let handle = Handle::new(sender);
        std::thread::scope(|scope| {
            let call =
                scope.spawn(|| response(RecoveryRoute::Submit, &sessions, &handle, &head, body));
            let Job::RecoverFromFloor { intent, reply, .. } = jobs
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
            else {
                panic!("only recovery was enqueued");
            };
            assert_eq!(intent.expected.issue_lsn.0, 9_007_199_254_740_993);
            assert_eq!(intent.expected.source_lsn.0, u64::MAX);
            drop(reply);
            let response = call.join().unwrap();
            assert_eq!(response.code, 202);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&response.body).unwrap(),
                serde_json::json!({"state":"unknown", "request_id":"8484848484848484848484848484848484848484848484848484848484848484"})
            );
            assert!(matches!(jobs.try_recv(), Err(mpsc::TryRecvError::Empty)));
        });
    }
}
