//! Conditioned memory append and authenticated applied-memory read.

use dudes_proto::ConditionedTarget;
use dudes_proto::rpc::memory::MemoryAppendParams;
use dudes_proto::rpc::memory::{
    MemoryReadParams, MemoryReadResult, MemorySearchOptions, MemorySearchParams,
};
use dudes_proto::rpc::{ErrorCode, Method, RpcError};
use serde_json::Value;
use std::sync::Arc;

use crate::auth::PeerRole;
use crate::conditioned::DecodedEffect;
use crate::dispatch::Call;
use crate::dispatch::error;
use crate::methods::refuse_nul;
use crate::methods::{MemoryRead, MemorySearch, encode, params};

/// Maximum decoded text in one whole-entry memory page.
pub const MAX_MEMORY_READ_TEXT_BYTES: usize = 65_536;

/// Read only the target selected by the authenticated role.
pub fn read<M: MemoryRead>(
    memory: Arc<M>,
) -> impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static {
    move |call| {
        let request: MemoryReadParams = params(&call)?;
        let (agent, caller) = match (call.role, request.agent) {
            (PeerRole::Agent { agent }, _) => (*agent, Some(*agent)),
            (PeerRole::Operator, Some(agent)) => (agent, None),
            _ => {
                return Err(error(
                    ErrorCode::InvalidParams,
                    "memory.read: an operator must name agent",
                ));
            }
        };
        let result: MemoryReadResult = memory.read(agent, caller, request.since_lsn)?;
        encode(Method::MemoryRead, &result)
    }
}

/// Validate a literal query and derive the target solely from the admitted role.
pub fn search<M: MemorySearch>(
    memory: Arc<M>,
) -> impl Fn(Call<'_>) -> Result<Value, RpcError> + Send + Sync + 'static {
    move |call| {
        let request: MemorySearchParams = params(&call)?;
        let options = MemorySearchOptions::new(request.query, request.since_lsn, request.limit)
            .map_err(|why| error(ErrorCode::InvalidParams, format!("memory.search: {why}")))?;
        let (agent, caller) = match (call.role, request.agent) {
            (PeerRole::Agent { agent }, target) if target.is_none_or(|target| target == *agent) => {
                (*agent, Some(*agent))
            }
            (PeerRole::Agent { .. }, _) => {
                return Err(error(
                    ErrorCode::InvalidParams,
                    "memory.search: an agent can search only itself",
                ));
            }
            (PeerRole::Operator, Some(agent)) => (agent, None),
            _ => {
                return Err(error(
                    ErrorCode::InvalidParams,
                    "memory.search: an operator must name agent",
                ));
            }
        };
        let result = memory.search(agent, caller, options)?;
        encode(Method::MemorySearch, &result)
    }
}

/// Decode the exact memory bytes and derive the target from the admitted caller kind.
///
/// A `text` holding U+0000 is invalid params. The memory digest enters every later
/// task of the target agent and nothing removes an entry, so one applied U+0000 would stop that
/// agent for good. It stops here, before the disposition, for every caller: nothing is
/// appended, not even a quarantine.
pub(crate) fn conditioned_append(
    value: Option<&Value>,
    role: &PeerRole,
) -> Result<DecodedEffect<MemoryAppendParams>, RpcError> {
    let request: MemoryAppendParams = serde_json::from_value(
        value
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
    )
    .map_err(|err| {
        error(
            ErrorCode::InvalidParams,
            format!("{}: {err}", Method::MemoryAppend.name()),
        )
    })?;

    let agent = match (role, request.agent) {
        (PeerRole::Agent { agent }, None) => *agent,
        (PeerRole::Agent { .. }, Some(_)) => {
            return Err(error(
                ErrorCode::InvalidParams,
                "memory.append: an agent must omit agent and can append only to itself",
            ));
        }
        (PeerRole::Operator, Some(agent)) => agent,
        (PeerRole::Operator, None) => {
            return Err(error(
                ErrorCode::InvalidParams,
                "memory.append: an operator must name agent explicitly",
            ));
        }
        (PeerRole::Unproven, _) => {
            return Err(error(
                ErrorCode::InvalidParams,
                "memory.append: the admitted caller has no memory target",
            ));
        }
    };
    refuse_nul(Method::MemoryAppend, &request.text)?;
    let payload = request.text.as_bytes().to_vec();
    Ok(DecodedEffect::new(
        request,
        ConditionedTarget::Agent { agent },
        Some(payload),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use dudes_proto::rpc::cred::OperatorCredential;
    use dudes_proto::rpc::memory::MemoryAppendParams;
    use dudes_proto::rpc::{ErrorCode, JSONRPC, Method, Request, RequestId, RpcError};
    use dudes_types::{AgentId, Lsn};

    use crate::auth::PeerProvenance;
    use crate::conditioned::PersistenceOperation;

    /// A U+0000 append, placement: it stops in the decoder the registration
    /// installs for `memory.append`, before the disposition. The call is an operator's
    /// without effect authority, which the table quarantines; it gets `-32602` and nothing is
    /// persisted, not even the digest-only quarantine.
    ///
    /// Control, first: the same call with U+0001 in its place persists exactly one disposition,
    /// the quarantine.
    ///
    /// Mutation: move the check behind the disposition, and the U+0000 call persists one too.
    #[test]
    fn a_nul_memory_append_stops_at_decode_before_any_disposition() {
        #[derive(Default)]
        struct Persisted(AtomicUsize);
        impl crate::methods::ConditionedMemoryControl for Persisted {
            fn append(
                &self,
                _: PersistenceOperation<'_, MemoryAppendParams>,
            ) -> Result<Lsn, RpcError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Lsn(1))
            }
        }

        let credential = || OperatorCredential::new("C".repeat(43)).expect("a credential");
        let persisted = Arc::new(Persisted::default());
        let mut dispatcher = crate::Dispatcher::new(crate::PeerClassifier::new(
            credential(),
            Arc::new(crate::testing::FakeMintedTokens::none()),
        ));
        crate::methods::register_memory_effect(&mut dispatcher, &persisted);
        let agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("an agent id");
        let call = |text: &str| {
            dispatcher
                .dispatch(
                    &Request {
                        jsonrpc: JSONRPC.to_owned(),
                        id: RequestId::Number(1),
                        method: Method::MemoryAppend.name().to_owned(),
                        params: Some(serde_json::json!({ "agent": agent, "text": text })),
                        v: None,
                        cred: Some(credential()),
                    },
                    PeerProvenance::NoRecordedAgent,
                    &Arc::new(crate::Outbound::sink()),
                )
                .error
                .expect("no call on this connection applies")
        };

        let quarantined = call("a\u{1}b");
        assert_eq!(
            quarantined.code,
            ErrorCode::Refused.code(),
            "{quarantined:?}"
        );
        assert_eq!(
            persisted.0.load(Ordering::SeqCst),
            1,
            "control: without U+0000 the quarantine is persisted"
        );

        let error = call("a\u{0}b");
        assert_eq!(
            persisted.0.load(Ordering::SeqCst),
            1,
            "decode stops before any disposition: {error:?}"
        );
        assert_eq!(error.code, ErrorCode::InvalidParams.code(), "{error:?}");
        assert_eq!(error.data, None, "invalid params carry no data");
        assert!(
            error.message.starts_with("memory.append:")
                && error.message.contains("U+0000")
                && error.message.contains("byte offset 1"),
            "the message names the method, U+0000 and the first offset: {}",
            error.message
        );
    }
}

#[cfg(test)]
mod read_tests {
    use super::*;
    use dudes_proto::rpc::{RequestId, Response};
    use dudes_types::{AgentId, Lsn};
    use std::sync::Mutex;

    #[derive(Clone)]
    struct Bytes(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Bytes {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct ReadCall {
        agent: AgentId,
        caller: Option<AgentId>,
        since_lsn: Option<Lsn>,
    }
    #[derive(Default)]
    struct Read(Mutex<Vec<ReadCall>>);
    impl MemoryRead for Read {
        fn read(
            &self,
            agent: AgentId,
            caller: Option<AgentId>,
            since_lsn: Option<Lsn>,
        ) -> Result<MemoryReadResult, RpcError> {
            self.0.lock().unwrap().push(ReadCall {
                agent,
                caller,
                since_lsn,
            });
            Ok(MemoryReadResult {
                text: "remember".into(),
                lsn: Lsn(7),
            })
        }
    }

    #[test]
    fn read_uses_the_proven_agent_and_an_explicit_operator_target() {
        let agent = AgentId::NIL;
        let other = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
        let reader = Arc::new(Read::default());
        let handler = read(Arc::clone(&reader));
        let outbound = Arc::new(crate::Outbound::sink());
        let params = serde_json::json!({"agent":other,"since_lsn":3});
        handler(Call {
            goal_replacement: None,
            method: Method::MemoryRead.into(),
            params: Some(&params),
            role: &PeerRole::Agent { agent },
            outbound: &outbound,
            hang_up: crate::server::HangUp::never(),
        })
        .unwrap();
        handler(Call {
            goal_replacement: None,
            method: Method::MemoryRead.into(),
            params: Some(&params),
            role: &PeerRole::Operator,
            outbound: &outbound,
            hang_up: crate::server::HangUp::never(),
        })
        .unwrap();
        assert_eq!(
            *reader.0.lock().unwrap(),
            vec![
                ReadCall {
                    agent,
                    caller: Some(agent),
                    since_lsn: Some(Lsn(3))
                },
                ReadCall {
                    agent: other,
                    caller: None,
                    since_lsn: Some(Lsn(3))
                }
            ]
        );
        assert!(
            handler(Call {
                goal_replacement: None,
                method: Method::MemoryRead.into(),
                params: None,
                role: &PeerRole::Operator,
                outbound: &outbound,
                hang_up: crate::server::HangUp::never(),
            })
            .is_err()
        );
    }

    #[test]
    fn a_maximum_escaped_memory_page_fits_a_complete_rpc_frame() {
        let result = MemoryReadResult {
            text: "\u{1}".repeat(MAX_MEMORY_READ_TEXT_BYTES),
            lsn: Lsn(u64::MAX),
        };
        let response = Response::ok(
            RequestId::Number(i64::MAX),
            serde_json::to_value(&result).unwrap(),
        );
        let mut bytes = Vec::new();
        crate::write_frame(&mut bytes, &response).unwrap();
        assert!(bytes.len() > MAX_MEMORY_READ_TEXT_BYTES * 6);
        assert!(bytes.len() < crate::MAX_FRAME_BYTES);
        let decoded: Response = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            serde_json::from_value::<MemoryReadResult>(decoded.result.unwrap()).unwrap(),
            result
        );
        let oversized = Response::ok(
            RequestId::String("i".repeat(crate::MAX_FRAME_BYTES)),
            serde_json::to_value(&result).unwrap(),
        );
        let mut refused = Vec::new();
        assert!(crate::write_frame(&mut refused, &oversized).is_err());
        assert!(refused.is_empty());
    }

    #[test]
    fn a_read_refuses_unproven_or_unresolvable_callers_before_the_port() {
        let agent = AgentId::NIL;
        let token = "t".repeat(43);
        let classifier = crate::PeerClassifier::new(
            dudes_proto::rpc::cred::OperatorCredential::new("C".repeat(43)).unwrap(),
            Arc::new(crate::testing::FakeMintedTokens::minted(&[(&token, agent)])),
        );
        let mut dispatcher = crate::Dispatcher::new(classifier);
        let reader = Arc::new(Read::default());
        crate::methods::register_memory_read(&mut dispatcher, &reader);
        let outbound = Arc::new(crate::Outbound::sink());
        for (params, provenance) in [
            (serde_json::json!({}), crate::PeerProvenance::Agent(agent)),
            (
                serde_json::json!({"token":token}),
                crate::PeerProvenance::Unresolvable,
            ),
        ] {
            let request = dudes_proto::rpc::Request {
                jsonrpc: dudes_proto::rpc::JSONRPC.into(),
                id: RequestId::Number(1),
                method: Method::MemoryRead.name().into(),
                params: Some(params),
                v: None,
                cred: None,
            };
            assert_eq!(
                dispatcher
                    .dispatch(&request, provenance, &outbound)
                    .error
                    .unwrap()
                    .code,
                ErrorCode::Unauthorized.code()
            );
        }
        assert!(reader.0.lock().unwrap().is_empty());
    }

    #[test]
    fn a_token_present_read_from_an_unknown_peer_is_refused_on_the_stream() {
        let agent = AgentId::NIL;
        let token = "t".repeat(43);
        let classifier = crate::PeerClassifier::new(
            dudes_proto::rpc::cred::OperatorCredential::new("C".repeat(43)).unwrap(),
            Arc::new(crate::testing::FakeMintedTokens::minted(&[(&token, agent)])),
        );
        let mut dispatcher = crate::Dispatcher::new(classifier);
        let reader = Arc::new(Read::default());
        crate::methods::register_memory_read(&mut dispatcher, &reader);
        let bytes = Bytes(Arc::new(Mutex::new(Vec::new())));
        let outbound = Arc::new(crate::Outbound::new(bytes.clone()));
        let request = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"memory.read","params":{"token":token}});
        crate::server::serve_stream(
            std::io::Cursor::new(format!("{request}\n")),
            &outbound,
            crate::auth::PeerIdentity::unknown(),
            crate::PeerProvenance::Unresolvable,
            &dispatcher,
        )
        .unwrap();
        let response: Response = serde_json::from_slice(&bytes.0.lock().unwrap()).unwrap();
        assert_eq!(response.error.unwrap().code, ErrorCode::Unauthorized.code());
        assert!(response.result.is_none());
        assert!(reader.0.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;
    use dudes_proto::rpc::cred::OperatorCredential;
    use dudes_proto::rpc::memory::MemorySearchResult;
    use dudes_proto::rpc::{JSONRPC, Request, RequestId};
    use dudes_types::{AgentId, Lsn};
    use std::sync::Mutex;

    #[derive(Debug, PartialEq, Eq)]
    struct SearchCall {
        agent: AgentId,
        caller: Option<AgentId>,
        query: String,
        since: Option<Lsn>,
        limit: usize,
    }
    #[derive(Default)]
    struct Search(Mutex<Vec<SearchCall>>);
    impl MemorySearch for Search {
        fn search(
            &self,
            agent: AgentId,
            caller: Option<AgentId>,
            options: MemorySearchOptions,
        ) -> Result<MemorySearchResult, RpcError> {
            self.0.lock().unwrap().push(SearchCall {
                agent,
                caller,
                query: options.query().into(),
                since: options.since_lsn(),
                limit: options.limit(),
            });
            Ok(MemorySearchResult::default())
        }
    }

    fn credential() -> OperatorCredential {
        OperatorCredential::new("C".repeat(43)).unwrap()
    }
    fn request(params: Value, operator: bool) -> Request {
        Request {
            jsonrpc: JSONRPC.into(),
            id: RequestId::Number(1),
            method: Method::MemorySearch.name().into(),
            params: Some(params),
            v: None,
            cred: operator.then(credential),
        }
    }
    fn dispatcher() -> (crate::Dispatcher, Arc<Search>, String) {
        let token = "t".repeat(43);
        let mut dispatcher = crate::Dispatcher::new(crate::PeerClassifier::new(
            credential(),
            Arc::new(crate::testing::FakeMintedTokens::minted(&[(
                &token,
                AgentId::NIL,
            )])),
        ));
        let search = Arc::new(Search::default());
        crate::methods::register_memory_search(&mut dispatcher, &search);
        assert_eq!(
            dispatcher.requirement(Method::MemorySearch),
            Some(crate::Requirement::OperatorOrAgent)
        );
        (dispatcher, search, token)
    }

    #[test]
    fn registered_search_derives_self_and_operator_targets_and_keeps_literal_query() {
        let (dispatcher, search, token) = dispatcher();
        let outbound = Arc::new(crate::Outbound::sink());
        let agent = AgentId::NIL;
        let other = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse::<AgentId>().unwrap();
        for target in [None, Some(agent)] {
            let result=dispatcher.dispatch(&request(serde_json::json!({"query":" \t🦀 ","agent":target,"since_lsn":u64::MAX,"limit":100,"token":token}),false),crate::PeerProvenance::Agent(agent),&outbound);
            assert!(result.error.is_none());
            assert_eq!(
                result.result,
                Some(serde_json::json!({"hits":[],"next_since_lsn":null}))
            );
        }
        assert!(
            dispatcher
                .dispatch(
                    &request(serde_json::json!({"query":"operator","agent":other}), true),
                    crate::PeerProvenance::NoRecordedAgent,
                    &outbound
                )
                .error
                .is_none()
        );
        assert_eq!(
            *search.0.lock().unwrap(),
            vec![
                SearchCall {
                    agent,
                    caller: Some(agent),
                    query: " \t🦀 ".into(),
                    since: Some(Lsn(u64::MAX)),
                    limit: 100
                },
                SearchCall {
                    agent,
                    caller: Some(agent),
                    query: " \t🦀 ".into(),
                    since: Some(Lsn(u64::MAX)),
                    limit: 100
                },
                SearchCall {
                    agent: other,
                    caller: None,
                    query: "operator".into(),
                    since: None,
                    limit: 10
                },
            ]
        );
    }

    #[test]
    fn registered_search_refuses_cross_target_and_invalid_inputs_without_calling_port() {
        let (dispatcher, search, token) = dispatcher();
        let outbound = Arc::new(crate::Outbound::sink());
        let other = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse::<AgentId>().unwrap();
        for params in [
            serde_json::json!({"query":"x","agent":other,"token":token}),
            serde_json::json!({"query":"a\u{0}b","token":token}),
            serde_json::json!({"query":"","token":token}),
            serde_json::json!({"query":"x".repeat(1025),"token":token}),
            serde_json::json!({"query":"x","limit":0,"token":token}),
            serde_json::json!({"query":"x","limit":101,"token":token}),
            serde_json::json!({"query":"x","typo":true,"token":token}),
        ] {
            let result = dispatcher.dispatch(
                &request(params, false),
                crate::PeerProvenance::Agent(AgentId::NIL),
                &outbound,
            );
            assert_eq!(result.error.unwrap().code, ErrorCode::InvalidParams.code());
            assert!(result.result.is_none());
            assert!(search.0.lock().unwrap().is_empty());
        }
        let result = dispatcher.dispatch(
            &request(serde_json::json!({"query":"x"}), true),
            crate::PeerProvenance::NoRecordedAgent,
            &outbound,
        );
        assert_eq!(result.error.unwrap().code, ErrorCode::InvalidParams.code());
        assert!(result.result.is_none());
        assert!(search.0.lock().unwrap().is_empty());
    }

    #[test]
    fn registered_search_refuses_unproven_and_unresolvable_identity_before_port() {
        let (dispatcher, search, token) = dispatcher();
        let outbound = Arc::new(crate::Outbound::sink());
        for (params, provenance) in [
            (
                serde_json::json!({"query":"x"}),
                crate::PeerProvenance::Agent(AgentId::NIL),
            ),
            (
                serde_json::json!({"query":"x","token":token}),
                crate::PeerProvenance::Unresolvable,
            ),
        ] {
            let result = dispatcher.dispatch(&request(params, false), provenance, &outbound);
            assert_eq!(result.error.unwrap().code, ErrorCode::Unauthorized.code());
            assert!(result.result.is_none());
        }
        assert!(search.0.lock().unwrap().is_empty());
    }
}
