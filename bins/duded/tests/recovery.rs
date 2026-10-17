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
        serde_json::from_slice(&bytes.0.lock().unwrap()).unwrap()
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

#[cfg(unix)]
#[path = "common/mod.rs"]
mod common;

#[cfg(unix)]
mod durable {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::Duration;

    use duded::settings::Seams;
    use dudes_journal::{Projections, Reader, Writer};
    use dudes_proto::record::samples;
    use dudes_proto::rpc::Method;
    use dudes_proto::rpc::recovery::{
        RecoveryAllowedChoices, RecoveryInspectResult, RecoveryReceiptState, RecoverySubmitParams,
        RecoveryWaitStatus,
    };
    use dudes_proto::rpc::system::PingResult;
    use dudes_proto::{Entry, Record, RecordKind};
    use dudes_rpc::client::Client;
    use dudes_supervisor::Rng;
    use dudes_types::{AgentId, GoalId, Lsn, Mode, ModelSelection, Provenance, Timestamp, TurnId};

    struct TrackedRng {
        inner: common::FixtureRng,
        drawn: Arc<Mutex<u128>>,
    }

    impl Rng for TrackedRng {
        fn ulid(&mut self) -> ulid::Ulid {
            let id = self.inner.ulid();
            *self.drawn.lock().unwrap() = self.inner.drawn();
            id
        }
        fn agent_token(&mut self) -> dudes_proto::rpc::hook::HookToken {
            let token = self.inner.agent_token();
            *self.drawn.lock().unwrap() = self.inner.drawn();
            token
        }
    }

    struct Backend {
        running: common::Running,
        fixture: common::Fixture,
        host: common::SharedHost,
        drawn: Arc<Mutex<u128>>,
        agent: AgentId,
        issue: Lsn,
        source: Lsn,
    }

    fn append(writer: &mut Writer, record: Record) -> Lsn {
        writer
            .append(&Entry {
                at: Timestamp::from_unix_millis(1_787_394_600_000),
                record,
            })
            .unwrap()
    }

    fn seed(fixture: &common::Fixture, settings: &duded::Settings) -> (AgentId, Lsn, Lsn) {
        let agent = AgentId::from_parts(91, 1);
        let goal = GoalId::from_parts(91, 2);
        let turn = TurnId::from_parts(91, 3);
        let (mut writer, _) =
            dudes_journal::open(&fixture.journal_dir(), settings.journal).unwrap();
        let mut hire = samples::record(RecordKind::AgentHired);
        let Record::AgentHired {
            id,
            cwd,
            mcp_servers,
            ..
        } = &mut hire
        else {
            unreachable!()
        };
        *id = agent;
        *cwd = fixture.state.path().to_path_buf();
        mcp_servers.clear();
        append(&mut writer, hire);
        let mut set = samples::record(RecordKind::GoalSet);
        let Record::GoalSet {
            agent: owner,
            goal: definition,
            ..
        } = &mut set
        else {
            unreachable!()
        };
        *owner = agent;
        definition.id = goal;
        append(&mut writer, set);
        let mut decision = samples::record(RecordKind::LoopDecided);
        let Record::LoopDecided {
            agent: owner,
            decision: action,
            ..
        } = &mut decision
        else {
            unreachable!()
        };
        *owner = agent;
        *action = dudes_proto::LoopDecision::Continue {
            task: "original owed work".into(),
        };
        let decision = append(&mut writer, decision);
        let issue = append(
            &mut writer,
            Record::TurnIssued {
                agent,
                turn,
                goal,
                mode: Mode::Managed,
                lsn_of_decision: decision,
                task: "original owed work".into(),
                session: None,
                model_selection: Some(ModelSelection::provider_default()),
                answers: Vec::new(),
            },
        );
        append(&mut writer, no_child_end(agent, turn));
        let source = append(
            &mut writer,
            Record::AgentHeld {
                agent,
                provenance: Provenance::Observed,
                cause: dudes_proto::AutomaticHold::LaunchFailed {
                    reason: "launch failed before a child existed".into(),
                },
            },
        );
        append(
            &mut writer,
            Record::HumanQuestion {
                agent,
                text: "Which output?".into(),
            },
        );
        (agent, issue, source)
    }

    fn no_child_end(agent: AgentId, turn: TurnId) -> Record {
        Record::TurnEnded {
            agent,
            turn,
            mode: Mode::Managed,
            provenance: Provenance::Observed,
            exit: dudes_proto::ExitStatus {
                code: None,
                signal: None,
            },
            usage: None,
            model_usage: BTreeMap::new(),
            last_message: None,
            session: None,
            outcome: Some(dudes_proto::TurnOutcome::NotLaunched {
                reason: "launch failed before a child existed".into(),
            }),
            elapsed_ms: None,
            final_reply: None,
            session_usage: None,
        }
    }

    fn backend(checkpoints: u64) -> Backend {
        let fixture = common::Fixture::new();
        let mut settings = fixture.settings();
        settings.journal.checkpoint_every = checkpoints;
        let (agent, issue, source) = seed(&fixture, &settings);
        let host = common::SharedHost::new();
        let seams = common::seams(
            &settings,
            host.clone(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            duded::startup::Witness::default(),
        );
        let drawn = Arc::new(Mutex::new(seams.rng.drawn()));
        let seams = Seams {
            host: seams.host,
            pty_host: seams.pty_host,
            predicates: seams.predicates,
            clock: seams.clock,
            rng: TrackedRng {
                inner: seams.rng,
                drawn: Arc::clone(&drawn),
            },
            resolver: seams.resolver,
            adapters: seams.adapters,
            children: seams.children,
            witness: seams.witness,
        };
        let daemon = duded::boot(&settings, seams).unwrap();
        let running = common::start(daemon);
        Backend {
            running,
            fixture,
            host,
            drawn,
            agent,
            issue,
            source,
        }
    }

    fn queued(
        jobs: &mpsc::Receiver<Job>,
    ) -> (
        Ask,
        mpsc::Sender<Result<Answered, dudes_proto::rpc::RpcError>>,
    ) {
        let Job::Ask { ask, reply } = jobs.recv_timeout(Duration::from_secs(10)).unwrap() else {
            panic!("one typed ask")
        };
        (ask, reply)
    }

    fn forward(
        backend: &Backend,
        dispatcher: &Arc<Dispatcher>,
        jobs: &mpsc::Receiver<Job>,
        method: &'static str,
        params: serde_json::Value,
    ) -> Response {
        let response = call(Arc::clone(dispatcher), method, params);
        let (ask, reply) = queued(jobs);
        reply.send(backend.running.handle.ask(ask)).unwrap();
        response.join().unwrap()
    }

    fn inspect(
        backend: &Backend,
        dispatcher: &Arc<Dispatcher>,
        jobs: &mpsc::Receiver<Job>,
    ) -> RecoverySubmitParams {
        let response = forward(
            backend,
            dispatcher,
            jobs,
            "recovery.inspect",
            json!({"agent":backend.agent}),
        );
        assert!(response.error.is_none(), "{response:?}");
        let inspection: RecoveryInspectResult =
            serde_json::from_value(response.result.unwrap()).unwrap();
        let RecoveryInspectResult::Available {
            expected,
            choices,
            waiting,
            reason,
            ..
        } = inspection
        else {
            panic!("available actual recovery")
        };
        assert_eq!(choices, RecoveryAllowedChoices::FreshOnly);
        assert_eq!(
            reason,
            dudes_proto::recovery::RecoveryReason::HeldOrPausedFailure
        );
        assert_eq!(waiting.open_question, RecoveryWaitStatus::Waiting);
        assert_eq!(expected.work.agent, backend.agent);
        assert_eq!(expected.issue_lsn, backend.issue);
        assert_eq!(expected.source_lsn, backend.source);
        assert_eq!(expected.expected_predecessor, None);
        RecoverySubmitParams {
            request_id: Hex32::new([81; 32]),
            expected: *expected,
            choice: dudes_proto::rpc::recovery::RecoveryChoice::FreshWithRecap,
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Snapshot {
        journal: BTreeMap<u32, Vec<u8>>,
        replayed_export: String,
        rng: u128,
        spawns: usize,
    }

    fn snapshot(backend: &Backend) -> Snapshot {
        let dir = backend.fixture.journal_dir();
        let journal = dudes_journal::segment::list_segments(&dir)
            .unwrap()
            .into_iter()
            .map(|id| {
                (
                    id,
                    std::fs::read(dudes_journal::segment::segment_path(&dir, id)).unwrap(),
                )
            })
            .collect();
        let mut projection = Projections::in_memory().unwrap();
        dudes_journal::replay(&Reader::new(&dir), &mut projection).unwrap();
        Snapshot {
            journal,
            replayed_export: projection.export().unwrap(),
            rng: *backend.drawn.lock().unwrap(),
            spawns: backend.host.spawned().len(),
        }
    }

    fn assert_published_caller_without_pump(backend: &Backend, checkpoints: u64) {
        let before = snapshot(backend);
        let entries = backend.fixture.entries();
        let (through, tail) = entries.last().unwrap();
        let (caller, _) = entries
            .iter()
            .rev()
            .find(|(_, entry)| !matches!(entry.record, Record::Checkpoint { .. }))
            .unwrap();
        if checkpoints == 0 {
            assert_eq!(caller, through);
        } else {
            assert_eq!(checkpoints, 1);
            let Record::Checkpoint {
                segment, offset, ..
            } = &tail.record
            else {
                panic!("cadence one must retain its actual checkpoint tail");
            };
            assert_eq!((*segment, *offset), (through.segment(), through.offset()));
            assert!(through > caller);
        }
        let settings = backend.fixture.settings();
        let mut client = Client::connect(settings.socket()).unwrap();
        let response = client.call(Method::SystemPing, None).unwrap();
        assert!(response.error.is_none(), "{response:?}");
        let ping: PingResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(ping.lsn, *caller);
        assert_eq!(snapshot(backend), before);
    }

    fn assert_claimed_inspection_is_inert(
        backend: &Backend,
        dispatcher: &Arc<Dispatcher>,
        jobs: &mpsc::Receiver<Job>,
        checkpoints: u64,
    ) {
        let before = snapshot(backend);
        let response = forward(
            backend,
            dispatcher,
            jobs,
            "recovery.inspect",
            json!({"agent":backend.agent}),
        );
        assert!(response.error.is_none(), "{response:?}");
        assert_eq!(
            serde_json::from_value::<RecoveryInspectResult>(response.result.unwrap()).unwrap(),
            RecoveryInspectResult::Unavailable {
                reason: dudes_proto::rpc::recovery::RecoveryInspectUnavailable::Claimed
            }
        );
        assert_published_caller_without_pump(backend, checkpoints);
        assert_eq!(snapshot(backend), before);
    }

    fn assert_pending(
        backend: &Backend,
        request: &RecoverySubmitParams,
        result: RecoverySubmitResult,
    ) -> RecoverySubmitResult {
        let RecoverySubmitResult::Accepted { receipt } = &result else {
            panic!("durable authorization: {result:?}")
        };
        assert_eq!(receipt.request_id, request.request_id);
        assert_eq!(receipt.status, RecoveryReceiptState::Pending);
        assert_eq!(receipt.choice, request.choice);
        let entries = backend.fixture.entries();
        let authorizations: Vec<_> = entries
            .iter()
            .filter_map(|(lsn, entry)| match &entry.record {
                Record::RecoveryAuthorized(value) => Some((*lsn, value)),
                _ => None,
            })
            .collect();
        assert_eq!(authorizations.len(), 1);
        assert_eq!(receipt.claim.authorization_lsn, authorizations[0].0);
        assert_eq!(authorizations[0].1.request_id, request.request_id);
        assert_eq!(authorizations[0].1.binding.work, request.expected.work);
        assert_eq!(
            authorizations[0].1.attribution,
            dudes_proto::recovery::RecoveryAttribution::LocalOperator {
                pid: 4242,
                uid: Some(501)
            }
        );
        assert_eq!(
            entries
                .iter()
                .filter(|(_, entry)| matches!(entry.record, Record::TurnIssued { .. }))
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .filter(|(_, entry)| matches!(entry.record, Record::TurnSpawned { .. }))
                .count(),
            0
        );
        assert_eq!(
            entries
                .iter()
                .filter(|(_, entry)| matches!(entry.record, Record::HumanQuestion { .. }))
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .filter(|(_, entry)| matches!(entry.record, Record::HumanAnswer { .. }))
                .count(),
            0
        );
        assert_eq!(backend.host.spawned().len(), 0);
        result
    }

    #[test]
    fn actual_pending_authorization_same_id_conflict_and_receipt_reads_preserve_the_cut() {
        for checkpoints in [0, 1] {
            let backend = backend(checkpoints);
            let (dispatcher, jobs) = fixture();
            let before_inspect = snapshot(&backend);
            let request = inspect(&backend, &dispatcher, &jobs);
            assert_eq!(snapshot(&backend), before_inspect);
            let response = forward(
                &backend,
                &dispatcher,
                &jobs,
                "recovery.submit",
                serde_json::to_value(request).unwrap(),
            );
            assert!(response.error.is_none(), "{response:?}");
            let accepted = assert_pending(
                &backend,
                &request,
                serde_json::from_value(response.result.unwrap()).unwrap(),
            );
            assert_published_caller_without_pump(&backend, checkpoints);
            let frozen = snapshot(&backend);
            assert!(frozen.rng > before_inspect.rng);
            assert_claimed_inspection_is_inert(&backend, &dispatcher, &jobs, checkpoints);
            for _ in 0..2 {
                let response = forward(
                    &backend,
                    &dispatcher,
                    &jobs,
                    "recovery.submit",
                    serde_json::to_value(request).unwrap(),
                );
                assert_published_caller_without_pump(&backend, checkpoints);
                assert_eq!(
                    serde_json::from_value::<RecoverySubmitResult>(response.result.unwrap())
                        .unwrap(),
                    accepted
                );
                assert_eq!(snapshot(&backend), frozen);
                let mut changed = serde_json::to_value(request).unwrap();
                changed["choice"] = json!("try_recorded");
                let response = forward(&backend, &dispatcher, &jobs, "recovery.submit", changed);
                assert_published_caller_without_pump(&backend, checkpoints);
                assert_eq!(
                    response.result,
                    Some(
                        json!({"state":"refused","reason":"request_conflict","prefix":{"required":null,"resume":null}})
                    )
                );
                assert_eq!(snapshot(&backend), frozen);
                let response = forward(
                    &backend,
                    &dispatcher,
                    &jobs,
                    "recovery.receipt",
                    json!({"request_id":request.request_id}),
                );
                assert_published_caller_without_pump(&backend, checkpoints);
                let RecoverySubmitResult::Accepted { receipt } = &accepted else {
                    unreachable!()
                };
                assert_eq!(
                    serde_json::from_value::<RecoveryReceiptResult>(response.result.unwrap())
                        .unwrap(),
                    RecoveryReceiptResult::Found {
                        receipt: receipt.clone()
                    }
                );
                assert_eq!(snapshot(&backend), frozen);
            }
            assert!(jobs.try_recv().is_err());
        }
    }

    #[test]
    fn a_lost_real_daemon_reply_is_recovered_only_by_id_from_durable_authorization() {
        let backend = backend(1);
        let (dispatcher, jobs) = fixture();
        let request = inspect(&backend, &dispatcher, &jobs);
        let response = call(
            Arc::clone(&dispatcher),
            "recovery.submit",
            serde_json::to_value(request).unwrap(),
        );
        let (ask, reply) = queued(&jobs);
        let answer = backend.running.handle.ask(ask).unwrap();
        let Answered::RecoverySubmitted(result) = answer else {
            panic!("real recovery result")
        };
        let accepted = assert_pending(&backend, &request, result);
        let frozen = snapshot(&backend);
        drop(reply);
        assert_eq!(
            response.join().unwrap().result,
            Some(json!({"state":"unknown","request_id":request.request_id}))
        );
        let response = call(
            Arc::clone(&dispatcher),
            "recovery.receipt",
            json!({"request_id":request.request_id}),
        );
        let (ask, reply) = queued(&jobs);
        let Ask::RecoveryReceipt {
            request: queried, ..
        } = &ask
        else {
            panic!("id-only lookup")
        };
        assert_eq!(*queried, request.request_id);
        reply.send(backend.running.handle.ask(ask)).unwrap();
        let RecoverySubmitResult::Accepted { receipt } = accepted else {
            unreachable!()
        };
        assert_eq!(
            serde_json::from_value::<RecoveryReceiptResult>(
                response.join().unwrap().result.unwrap()
            )
            .unwrap(),
            RecoveryReceiptResult::Found { receipt }
        );
        assert_eq!(snapshot(&backend), frozen);
        assert!(jobs.try_recv().is_err());
    }
}
