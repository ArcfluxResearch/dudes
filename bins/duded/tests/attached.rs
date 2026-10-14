//! The connection-bound `agent.attach` path through the production dispatcher and daemon loop.

#![cfg(unix)]

mod common;

use std::num::NonZeroUsize;
use std::sync::Arc;

use common::{Fixture, ScriptedColumn, ScriptedRunner, SharedHost, SharedPtyHost, TestClock};
use duded::startup::Witness;
use dudes_proto::rpc::agent::{AttachParams, AttachResult};
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::{AgentRef, Method};
use dudes_proto::{Actor, PeerProvenance, Record};
use dudes_rpc::{Client, LocalClient};
use dudes_supervisor::Input;
use dudes_supervisor::testing::FakeRng;
use dudes_types::AgentId;

fn credential_of(settings: &duded::Settings) -> OperatorCredential {
    let text = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("the daemon minted an operator credential");
    OperatorCredential::new(text).expect("the credential is well formed")
}

fn attach(
    client: &mut LocalClient,
    settings: &duded::Settings,
    agent: AgentId,
) -> dudes_proto::rpc::Response {
    attach_sized(client, settings, agent, 101, 37)
}

fn attach_sized(
    client: &mut LocalClient,
    settings: &duded::Settings,
    agent: AgentId,
    cols: u16,
    rows: u16,
) -> dudes_proto::rpc::Response {
    client
        .call_as_operator(
            Method::AgentAttach,
            Some(serde_json::to_value(AttachParams { agent, cols, rows }).unwrap()),
            credential_of(settings),
        )
        .expect("attach response")
}

fn detach(
    client: &mut LocalClient,
    settings: &duded::Settings,
    agent: AgentId,
) -> dudes_proto::rpc::Response {
    client
        .call_as_operator(
            Method::AgentDetach,
            Some(serde_json::to_value(AgentRef { agent }).unwrap()),
            credential_of(settings),
        )
        .expect("detach response")
}

#[test]
fn unsupported_native_queries_stay_unknown_after_a_pid_is_reported_gone() {
    use dudes_supervisor::host::{
        NativeBoundaryAcknowledgement, NativeBoundaryReceipt, OwnedTargetsStatus, ProcessHost,
    };
    let mut host = SharedHost::new();
    let key = dudes_supervisor::host::AdoptionKey {
        pid: 70001,
        start_time: 900,
        boot: None,
    };
    host.answer(key.pid, dudes_supervisor::ProbeAnswer::NoSuchPid);
    assert!(matches!(
        host.owned_targets(1).status,
        OwnedTargetsStatus::UnknownUnsupported
    ));
    assert_eq!(
        host.acknowledge_native_boundary_absence(&key, &NativeBoundaryReceipt::fresh()),
        NativeBoundaryAcknowledgement::UnknownUnsupported
    );
    assert_eq!(host.shutdowns(), 0);
}

#[test]
fn managed_native_ack_cannot_reuse_a_historical_settlement_for_the_same_full_key() {
    use dudes_supervisor::host::{NativeBoundaryAcknowledgement, ProcessHost, SpawnResult};
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let Hired {
        mut running, host, ..
    } = hired(&fixture, &ptys, fixture.settings());
    let old_key = fixture.await_record("settled prior managed spawn", |record| match record {
        Record::TurnSpawned {
            pid,
            start_time,
            boot,
            ..
        } => Some(dudes_supervisor::host::AdoptionKey {
            pid: *pid,
            start_time: *start_time,
            boot: boot.clone(),
        }),
        _ => None,
    });
    assert!(host.native_acknowledged(&old_key));
    let spec = host.spawned().into_iter().next().unwrap();
    running.stop(duded::ExitCause::Ended);
    let historical = fixture.entries();
    assert!(historical.iter().any(|(_, entry)| matches!(
        entry.record,
        Record::InstructionAttemptBoundarySettled { .. }
    )));
    let mut fresh = SharedHost::with_native_evidence(fixture.journal_dir());
    let SpawnResult::Spawned(key) =
        fresh.spawn(&spec, &dudes_providers::adapter::ManagedInstall::default())
    else {
        panic!("a new mock generation must spawn")
    };
    assert_eq!(
        (key.pid, key.start_time, &key.boot),
        (old_key.pid, old_key.start_time, &old_key.boot)
    );
    fresh.mark_native_reaped(&key);
    let queried = fresh.owned_targets(1);
    assert_eq!(queried.targets.len(), 1);
    let receipt = queried.targets[0].native_absence_receipt.as_ref().unwrap();
    assert_eq!(
        fixture.entries(),
        historical,
        "this generation has no new journal receipts"
    );
    assert_eq!(
        fresh.acknowledge_native_boundary_absence(&key, receipt),
        NativeBoundaryAcknowledgement::RefusedUnobserved
    );
    assert_eq!(
        fresh.owned_targets(1).targets.len(),
        1,
        "the fresh query target remains retained"
    );
    assert!(!fresh.native_acknowledged(&key));
    assert_eq!(fresh.shutdowns(), 0);
}

#[test]
fn managed_native_ack_keeps_its_start_when_another_agent_starts_later() {
    use dudes_supervisor::host::{NativeBoundaryAcknowledgement, ProcessHost};
    let mut interleaved = ManagedInterleaving::new();
    interleaved.settle(interleaved.own, interleaved.own_started);
    assert_eq!(
        interleaved
            .host
            .acknowledge_native_boundary_absence(&interleaved.key, &interleaved.receipt),
        NativeBoundaryAcknowledgement::Acknowledged,
        "the later unrelated start cannot replace this launch's exact baseline"
    );
    assert!(interleaved.host.native_acknowledged(&interleaved.key));
    assert!(interleaved.host.owned_targets(1).targets.is_empty());
}

#[test]
fn managed_native_ack_refuses_an_unrelated_interleaved_settlement() {
    use dudes_supervisor::host::{NativeBoundaryAcknowledgement, ProcessHost};
    let mut interleaved = ManagedInterleaving::new();
    interleaved.settle(interleaved.unrelated, interleaved.unrelated_started);
    assert_eq!(
        interleaved
            .host
            .acknowledge_native_boundary_absence(&interleaved.key, &interleaved.receipt),
        NativeBoundaryAcknowledgement::RefusedUnobserved,
        "an unrelated owner cannot authorize this launch even with the same full key"
    );
    assert!(!interleaved.host.native_acknowledged(&interleaved.key));
    assert_eq!(interleaved.host.owned_targets(1).targets.len(), 1);
    interleaved.settle(interleaved.own, interleaved.own_started);
    assert_eq!(
        interleaved
            .host
            .acknowledge_native_boundary_absence(&interleaved.key, &interleaved.receipt),
        NativeBoundaryAcknowledgement::Acknowledged,
        "the same retained receipt can retry against its own durable settlement"
    );
}

struct ManagedInterleaving {
    fixture: Fixture,
    writer: dudes_journal::Writer,
    at: dudes_types::Timestamp,
    host: SharedHost,
    key: dudes_supervisor::host::AdoptionKey,
    receipt: dudes_supervisor::host::NativeBoundaryReceipt,
    own: dudes_proto::InstructionObservationOwner,
    unrelated: dudes_proto::InstructionObservationOwner,
    own_started: dudes_types::Lsn,
    unrelated_started: dudes_types::Lsn,
}

impl ManagedInterleaving {
    fn source() -> (
        AgentId,
        AgentId,
        dudes_proto::Entry,
        dudes_proto::Entry,
        dudes_providers::launch::LaunchSpec,
    ) {
        use dudes_proto::InstructionObservationOwner;
        let source = Fixture::new();
        let ptys = SharedPtyHost::with_native_evidence(source.journal_dir());
        let Hired {
            mut running,
            host,
            agent,
            ..
        } = hired(&source, &ptys, source.settings());
        let other = common::hire(&running.handle, source.state.path());
        running.stop(duded::ExitCause::Ended);
        let entries = source.entries();
        let start = entries
            .iter()
            .find_map(|(_, entry)| match &entry.record {
                Record::InstructionObservationStarted {
                    owner: InstructionObservationOwner::Managed { agent: seen, .. },
                    ..
                } if *seen == agent => Some(entry.clone()),
                _ => None,
            })
            .unwrap();
        let issue = entries
            .iter()
            .find_map(|(_, entry)| match &entry.record {
                Record::TurnIssued { agent: seen, .. } if *seen == agent => Some(entry.clone()),
                _ => None,
            })
            .unwrap();
        let spec = host.spawned().into_iter().next().unwrap();
        (agent, other, start, issue, spec)
    }

    fn new() -> Self {
        use dudes_proto::{InstructionIssueAssociation, InstructionObservationOwner};
        use dudes_supervisor::host::{ProcessHost, SpawnResult};
        let (agent, other, start, issue, mut spec) = Self::source();
        let fixture = Fixture::new();
        spec.cwd = fixture.state.path().to_path_buf();
        for var in &mut spec.env {
            if var.name() == "DUDES_MAILBOX" {
                *var = dudes_providers::launch::EnvVar::new(
                    "DUDES_MAILBOX",
                    fixture.state.path().join("agents").join(agent.to_string()),
                );
            }
        }
        let options = dudes_journal::JournalOptions {
            fsync: dudes_journal::Fsync::Full,
            checkpoint_every: 0,
            ..dudes_journal::JournalOptions::default()
        };
        let (mut writer, _) = dudes_journal::open(&fixture.journal_dir(), options).unwrap();
        let own = InstructionObservationOwner::Managed {
            agent,
            turn: dudes_types::TurnId::parse("01ARZ3NDEKTSV4RRFFQ69G5FBD").unwrap(),
            issue: InstructionIssueAssociation::Prospective,
        };
        let unrelated = InstructionObservationOwner::Managed {
            agent: other,
            turn: dudes_types::TurnId::parse("01ARZ3NDEKTSV4RRFFQ69G5FBE").unwrap(),
            issue: InstructionIssueAssociation::Prospective,
        };
        let mut append_start = |owner| {
            let InstructionObservationOwner::Managed { agent, turn, .. } = owner else {
                unreachable!()
            };
            let mut start = start.clone();
            let Record::InstructionObservationStarted { owner: target, .. } = &mut start.record
            else {
                unreachable!()
            };
            *target = owner;
            let started = writer.append_required(&start).unwrap();
            let mut issue = issue.clone();
            let Record::TurnIssued {
                agent: target_agent,
                turn: target_turn,
                ..
            } = &mut issue.record
            else {
                unreachable!()
            };
            *target_agent = agent;
            *target_turn = turn;
            writer.append(&issue).unwrap();
            started
        };
        let own_started = append_start(own);
        let unrelated_started = append_start(unrelated);
        assert!(own_started < unrelated_started);
        assert_eq!(
            fixture.entries().len(),
            4,
            "the complete interleaved prefix is readable"
        );
        let mut host = SharedHost::with_native_evidence(fixture.journal_dir());
        let SpawnResult::Spawned(key) =
            host.spawn(&spec, &dudes_providers::adapter::ManagedInstall::default())
        else {
            panic!("the delayed mock launch must spawn")
        };
        host.mark_native_reaped(&key);
        let queried = host.owned_targets(1);
        let receipt = queried.targets[0]
            .native_absence_receipt
            .as_ref()
            .unwrap()
            .clone();
        Self {
            fixture,
            writer,
            at: start.at,
            host,
            key,
            receipt,
            own,
            unrelated,
            own_started,
            unrelated_started,
        }
    }

    fn settle(
        &mut self,
        owner: dudes_proto::InstructionObservationOwner,
        started_lsn: dudes_types::Lsn,
    ) {
        use dudes_supervisor::host::ProcessHost;
        let dudes_proto::InstructionObservationOwner::Managed { agent, turn, .. } = owner else {
            unreachable!()
        };
        let spawned = self
            .writer
            .append(&dudes_proto::Entry {
                at: self.at,
                record: Record::TurnSpawned {
                    agent,
                    turn,
                    pid: self.key.pid,
                    start_time: self.key.start_time,
                    boot: self.key.boot.clone(),
                    session: None,
                    reissued: false,
                    mcp_grant: None,
                },
            })
            .unwrap();
        self.writer
            .append_required(&dudes_proto::Entry {
                at: self.at,
                record: Record::InstructionAttemptBoundarySettled {
                    owner,
                    started_lsn,
                    proof:
                        dudes_proto::InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                            receipt: dudes_proto::InstructionNativeBoundaryReceipt::Spawned {
                                lsn: spawned,
                            },
                        },
                },
            })
            .unwrap();
        assert!(started_lsn < spawned);
        assert!(
            matches!(self.fixture.records().last(), Some(Record::InstructionAttemptBoundarySettled {
            owner: seen, started_lsn: seen_start, ..
        }) if *seen == owner && *seen_start == started_lsn),
            "the exact complete settlement is readable before ACK"
        );
        assert_eq!(self.host.owned_targets(1).targets.len(), 1);
    }
}

#[test]
fn pty_native_ack_cannot_reuse_a_historical_settlement_for_the_same_handle_and_key() {
    use duded::ports::{PtyCloseOutcome, PtyHostPort};
    let fixture = Fixture::new();
    let mut prior = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &prior);
    let old_key = prior.native_key(session.pty).unwrap();
    let old_token = prior.track_native_close(session.pty).unwrap();
    let (spec, cols, rows) = prior.opens().into_iter().next().unwrap();
    assert!(
        detach(&mut session.owner, &session.settings, session.agent)
            .error
            .is_none()
    );
    prior.await_native_acknowledged(session.pty);
    session.running.stop(duded::ExitCause::Ended);
    let historical = fixture.entries();
    let mut fresh = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let opened = fresh.open(spec, cols, rows).unwrap();
    assert_eq!(opened.pty, session.pty);
    let key = fresh.native_key(opened.pty).unwrap();
    assert_eq!(
        (key.pid, key.start_time, &key.boot),
        (old_key.pid, old_key.start_time, &old_key.boot)
    );
    let token = fresh.track_native_close(opened.pty).unwrap();
    assert_ne!(token, old_token);
    assert!(matches!(fresh.close(opened.pty), PtyCloseOutcome::Closed));
    assert!(fresh.native_close_absence(opened.pty, &token).is_ok());
    assert!(!fresh.acknowledge_native_close(opened.pty, &old_token));
    assert_eq!(
        fixture.entries(),
        historical,
        "only the prior generation's settlement is durable"
    );
    assert!(!fresh.acknowledge_native_close(opened.pty, &token));
    assert_eq!(
        fresh.track_native_close(opened.pty).unwrap(),
        token,
        "the exact fresh query survives a refused acknowledgement"
    );
    assert!(!fresh.native_acknowledged(opened.pty));
}

#[test]
fn a_keyless_confirmed_pty_close_does_not_authorize_a_replacement_attachment() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let fixed =
        duded::classify::Resolver::fixed(ScriptedColumn::new(PeerProvenance::NoRecordedAgent));
    let Hired {
        settings,
        mut running,
        agent,
        mut owner,
        ..
    } = hired_with_resolver(&fixture, &ptys, fixture.settings(), fixed);
    let response = attach(&mut owner, &settings, agent);
    assert!(response.error.is_none(), "{response:?}");
    let attached: AttachResult = serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(!fixture.records().iter().any(|record| matches!(record,
        Record::PtySpawned { agent: owned, .. } if *owned == agent)));
    assert!(detach(&mut owner, &settings, agent).error.is_none());
    assert!(fixture.records().iter().any(|record| matches!(record,
        Record::InstructionObservationFinished {
            owner: dudes_proto::InstructionObservationOwner::AttachedProcess { agent: found, .. },
            settlement: dudes_proto::InstructionSettlement::Unknown {
                reason: dudes_proto::InstructionUnknownReason::UnrecordedLaunchBoundary,
                end: None,
                sampled_at: None,
                recovery: false,
                comparison,
            }, ..
        } if *found == agent && comparison.changes.is_empty()
            && comparison.gaps.iter().any(|gap| gap.reason == dudes_proto::InstructionGapKind::UnrecordedLaunchBoundary))));
    assert!(!ptys.native_acknowledged(attached.pty));
    assert!(!fixture.records().iter().any(|record| matches!(record,
        Record::InstructionAttemptBoundarySettled {
            owner: dudes_proto::InstructionObservationOwner::AttachedProcess { agent: owned, .. }, ..
        } if *owned == agent)));
    assert!(attach(&mut owner, &settings, agent).error.is_some());
    assert_eq!(ptys.opens().len(), 1);
    running.stop(duded::ExitCause::Ended);
}

#[test]
fn attached_claude_inherits_context_and_refuses_a_live_peer_before_pty_open() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut seams = common::seams_with(
        host.clone(),
        Arc::new(ScriptedRunner::scripted([common::passes()])),
        Arc::new(TestClock::new()),
        Witness::default(),
        ScriptedColumn::new(PeerProvenance::NoRecordedAgent),
    );
    seams.pty_host = Box::new(ptys.clone());
    seams.resolver = common::native_resolver(&ptys);
    host.bind_child_ends(Arc::clone(&seams.children));
    let daemon = duded::boot(&settings, seams).unwrap();
    let mut running = common::start(daemon);
    let mut client = Client::connect(settings.socket()).unwrap();
    let candidate = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(candidate, &["/usr/bin/true"]),
    );
    let (turn, session) = fixture.await_record("candidate issue", |record| match record {
        Record::TurnIssued {
            agent,
            turn,
            session: Some(session),
            ..
        } if *agent == candidate => Some((*turn, session.clone())),
        _ => None,
    });
    running
        .handle
        .input(Input::SessionLearned { turn, session });
    fixture.await_record("candidate confirmed session", |record| match record {
        Record::SessionLearned { turn: seen, .. } if *seen == turn => Some(()),
        _ => None,
    });
    common::end_turn_with_native(&fixture, &running.handle, &host, turn);
    fixture.await_record("candidate observed completion", |record| match record {
        Record::LoopDecided {
            agent,
            decision: dudes_proto::LoopDecision::Done,
            ..
        } if *agent == candidate => Some(()),
        _ => None,
    });
    let peer = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(peer, &["/usr/bin/false"]),
    );
    let pid = fixture.await_record("live peer child", |record| match record {
        Record::TurnSpawned { agent, pid, .. } if *agent == peer => Some(*pid),
        _ => None,
    });
    let response = attach(&mut client, &settings, candidate);
    let refusal = response
        .error
        .expect("attached composition refuses live shared context even under managed default");
    assert!(refusal.message.contains(&peer.to_string()), "{refusal:?}");
    assert!(refusal.message.contains("worktree"), "{refusal:?}");
    assert_eq!(ptys.opens().len(), 0);
    assert!(fixture.records().iter().any(|record| matches!(record,
        Record::AgentHeld { agent, cause: dudes_proto::AutomaticHold::LaunchRefused { reason }, .. }
            if *agent == candidate && reason.contains(&peer.to_string()))));

    let peer_turn = fixture.await_record("exact live peer turn", |record| match record {
        Record::TurnSpawned {
            agent,
            turn,
            pid: spawned,
            ..
        } if *agent == peer && *spawned == pid => Some(*turn),
        _ => None,
    });
    common::end_turn_with_native(&fixture, &running.handle, &host, peer_turn);
    running
        .handle
        .ask(duded::ports::Ask::Resume(candidate))
        .unwrap();
    let response = attach(&mut client, &settings, candidate);
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(ptys.opens().len(), 1);
    assert!(detach(&mut client, &settings, candidate).error.is_none());
    running.stop(duded::ExitCause::ShutdownRpc);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the selector holds one connection across session establishment, attach, and cleanup"
)]
fn agent_attach_binds_the_durable_attachment_to_one_connection() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let children = duded::ChildEnds::new();
    host.bind_child_ends(Arc::clone(&children));
    let seams = duded::Seams {
        host: host.clone(),
        pty_host: Box::new(ptys.clone()),
        predicates: Arc::new(ScriptedRunner::scripted([common::passes()])),
        clock: Arc::new(TestClock::new()),
        rng: FakeRng::new(),
        resolver: common::native_resolver(&ptys),
        adapters: dudes_providers::registry::Adapters::from_config(
            &dudes_providers::registry::ProvidersConfig {
                claude: Some(dudes_providers::claude::config::ClaudeConfig::default()),
            },
            &[],
        )
        .expect("default Claude adapter"),
        children,
        witness: Witness::default(),
    };
    let daemon = duded::boot(&settings, seams).expect("boot");
    let mut running = common::start(daemon);

    // This connection is accepted in the pre-agent epoch and keeps that lease after hire seals it.
    let mut client = Client::connect(settings.socket()).expect("connect before the first hire");
    let mut second = Client::connect(settings.socket()).expect("second pre-agent connection");
    let mut third = Client::connect(settings.socket()).expect("third pre-agent connection");
    let ping = second
        .call(Method::SystemPing, None)
        .expect("second connection is accepted before hire");
    assert!(ping.error.is_none());
    assert!(
        third
            .call(Method::SystemPing, None)
            .expect("third connection is accepted before hire")
            .error
            .is_none()
    );
    let hire = client
        .call_as_operator(
            Method::AgentHire,
            Some(serde_json::to_value(common::hire_params(fixture.state.path())).unwrap()),
            credential_of(&settings),
        )
        .expect("hire response");
    assert!(hire.error.is_none(), "hire was refused: {hire:?}");
    let hired: dudes_proto::rpc::agent::HireResult =
        serde_json::from_value(hire.result.expect("hire result")).unwrap();
    let attached_before = fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::Attached { .. }))
        .count();
    assert!(
        attach(&mut client, &settings, hired.agent).error.is_some(),
        "an agent with no confirmed session is not attachable"
    );
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::Attached { .. }))
            .count(),
        attached_before
    );
    assert!(ptys.opens().is_empty());

    let goal = client
        .call_as_operator(
            Method::GoalSet,
            Some(
                serde_json::to_value(common::goal_params(hired.agent, &["/usr/bin/true"])).unwrap(),
            ),
            credential_of(&settings),
        )
        .expect("goal response");
    assert!(goal.error.is_none(), "goal was refused: {goal:?}");

    let (turn, session) = fixture.await_record("TurnIssued", |record| match record {
        Record::TurnIssued {
            turn,
            session: Some(session),
            ..
        } => Some((*turn, session.clone())),
        _ => None,
    });
    running.handle.input(Input::SessionLearned {
        turn,
        session: session.clone(),
    });
    fixture.await_record("SessionLearned", |record| match record {
        Record::SessionLearned { turn: seen, .. } if *seen == turn => Some(()),
        _ => None,
    });
    common::end_turn_with_native(&fixture, &running.handle, &host, turn);
    fixture.await_record("TurnEnded", |record| match record {
        Record::TurnEnded { turn: seen, .. } if *seen == turn => Some(()),
        _ => None,
    });
    fixture.await_record("PredicateResult", |record| {
        matches!(record, Record::PredicateResult { .. }).then_some(())
    });

    let attached_before = fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::Attached { .. }))
        .count();
    assert!(
        attach_sized(&mut client, &settings, hired.agent, 0, 24)
            .error
            .is_some(),
        "invalid dimensions are refused before the supervisor command"
    );
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::Attached { .. }))
            .count(),
        attached_before
    );
    assert!(ptys.opens().is_empty());

    let response = attach(&mut client, &settings, hired.agent);
    assert!(response.error.is_none(), "attach was refused: {response:?}");
    let attached: AttachResult =
        serde_json::from_value(response.result.expect("attach result")).unwrap();
    assert_eq!(attached.pty.0, 41, "the host's exact id is returned");
    assert!(
        ptys.attached_before_open(),
        "Attached was durable before open"
    );

    let opens = ptys.opens();
    assert_eq!(opens.len(), 1);
    assert_eq!((opens[0].1, opens[0].2), (101, 37));
    assert_eq!(opens[0].0.stdin, dudes_providers::launch::LaunchInput::Null);
    assert!(opens[0].0.argv.iter().any(|part| part == "--resume"));
    assert!(opens[0].0.argv.iter().any(|part| part == session.as_str()));
    let record = fixture
        .records()
        .into_iter()
        .find(|record| matches!(record, Record::Attached { .. }))
        .expect("durable Attached");
    assert!(matches!(
        record,
        Record::Attached {
            agent,
            by: Actor::Peer { .. },
            provenance: Some(PeerProvenance::NoRecordedAgent),
            ..
        } if agent == hired.agent
    ));

    let attached_records = fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::Attached { .. }))
        .count();
    assert!(
        attach(&mut client, &settings, hired.agent).error.is_some(),
        "one connection cannot acquire a second live slot"
    );
    assert!(
        attach(&mut second, &settings, hired.agent).error.is_some(),
        "another connection cannot replace the live agent attachment"
    );
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::Attached { .. }))
            .count(),
        attached_records,
        "both ownership refusals happen before another Attached"
    );

    drop(client);
    fixture.await_record("system Detached", |record| match record {
        Record::Detached {
            agent,
            by: Actor::System,
            provenance: None,
        } if *agent == hired.agent => Some(()),
        _ => None,
    });
    assert_eq!(ptys.closes(), vec![attached.pty]);
    ptys.await_native_acknowledged(attached.pty);
    assert!(fixture.records().iter().any(|record| matches!(record,
        Record::InstructionAttemptBoundarySettled {
            owner: dudes_proto::InstructionObservationOwner::AttachedProcess { agent, .. }, ..
        } if *agent == hired.agent)));

    let replacement = attach(&mut second, &settings, hired.agent);
    assert!(
        replacement.error.is_none(),
        "the released agent can attach again"
    );
    let replacement: AttachResult =
        serde_json::from_value(replacement.result.expect("replacement result")).unwrap();
    assert_eq!(replacement.pty.0, 42);

    let mut late = Client::connect(settings.socket()).expect("post-hire connection");
    let suppressed = attach(&mut late, &settings, hired.agent);
    assert!(
        suppressed.error.is_some(),
        "sealed authority suppresses Apply"
    );
    assert!(fixture.records().iter().any(|record| matches!(
        record,
        Record::ConditionedEffectSuppressed {
            method: dudes_proto::ConditionedMethod::AgentAttach,
            reason,
            ..
        } if reason == "operator authority unavailable"
    )));
    drop(late);
    drop(second);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::Detached { .. }))
        .count()
        < 2
    {
        assert!(
            std::time::Instant::now() < deadline,
            "replacement was detached"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(ptys.closes(), vec![attached.pty, replacement.pty]);
    ptys.await_native_acknowledged(replacement.pty);

    {
        let retained_fixture = Fixture::new();
        let retained_ptys = SharedPtyHost::with_native_evidence(retained_fixture.journal_dir());
        let Hired {
            mut running,
            settings,
            agent,
            mut owner,
            ..
        } = self::hired(
            &retained_fixture,
            &retained_ptys,
            retained_fixture.settings(),
        );
        retained_ptys.retain_next_failed_open();
        retained_ptys.fail_next_close();
        let uncertain = attach(&mut owner, &settings, agent);
        assert!(
            uncertain.error.is_some(),
            "cleanup uncertainty is never returned as attached"
        );
        assert_eq!(retained_ptys.opens().len(), 1);
        assert!(
            retained_ptys
                .live()
                .contains(&dudes_proto::rpc::pty::PtyId(41))
        );
        drop(owner);
        assert_eq!(
            running.stop(duded::ExitCause::Ended),
            duded::ExitCause::Ended
        );
        assert_eq!(
            retained_ptys.closes(),
            vec![
                dudes_proto::rpc::pty::PtyId(41),
                dudes_proto::rpc::pty::PtyId(41)
            ],
            "uncertain cleanup stays reachable for the exit retry"
        );
        assert!(!retained_ptys.native_acknowledged(dudes_proto::rpc::pty::PtyId(41)));
    }

    let before_failed = fixture.entries().last().unwrap().0;
    ptys.fail_next_open();
    let failed = attach(&mut third, &settings, hired.agent);
    assert!(failed.error.is_some(), "a failed open returns no PTY id");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::Detached { .. }))
        .count()
        < 3
    {
        assert!(
            std::time::Instant::now() < deadline,
            "a confirmed failed open was system-detached"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let (failed_started, failed_owner) = fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| match entry.record {
            Record::InstructionObservationStarted {
                owner:
                    owner @ dudes_proto::InstructionObservationOwner::AttachedProcess { agent, .. },
                ..
            } if lsn > before_failed && agent == hired.agent => Some((lsn, owner)),
            _ => None,
        })
        .expect("the failed physical open has its own durable starting boundary");
    let failed_finished =
        fixture.await_record("exact failed-open Unknown", |record| match record {
            Record::InstructionObservationFinished {
                owner,
                baseline: dudes_proto::InstructionBaselineRef::Started { lsn },
                settlement:
                    dudes_proto::InstructionSettlement::Unknown {
                        reason: dudes_proto::InstructionUnknownReason::UnrecordedLaunchBoundary,
                        end: None,
                        sampled_at: None,
                        ..
                    },
                ..
            } if *owner == failed_owner && *lsn == failed_started => Some(record.clone()),
            _ => None,
        });
    let assert_failed_unresolved = || {
        let mut instructions = dudes_proto::instruction_state::InstructionState::default();
        let entries = fixture.entries();
        for (lsn, entry) in &entries {
            instructions
                .apply_validated(*lsn, entry, |bytes| {
                    dudes_proto::record::Hex32::new(*blake3::hash(bytes).as_bytes())
                })
                .expect("fold the actual validated durable prefix");
        }
        let attempt = instructions.attempt(failed_started).unwrap();
        assert_eq!(attempt.owner, failed_owner);
        assert!(attempt.spawn_lsn.is_none());
        assert!(attempt.boundary_settled.is_none());
        assert!(matches!(
            &attempt.finished.as_ref().unwrap().settlement,
            dudes_proto::InstructionSettlement::Unknown {
                reason: dudes_proto::InstructionUnknownReason::UnrecordedLaunchBoundary,
                end: None,
                sampled_at: None,
                ..
            }
        ));
        assert_eq!(
            instructions
                .unresolved_launch_attempts(hired.agent)
                .map(|(lsn, _)| *lsn)
                .collect::<Vec<_>>(),
            vec![failed_started]
        );
        let finishes: Vec<_> = entries
            .iter()
            .filter_map(|(_, entry)| match &entry.record {
                Record::InstructionObservationFinished {
                    baseline: dudes_proto::InstructionBaselineRef::Started { lsn },
                    ..
                } if *lsn == failed_started => Some(&entry.record),
                _ => None,
            })
            .collect();
        assert_eq!(
            finishes,
            vec![&failed_finished],
            "the original failed-open Unknown remains immutable"
        );
        assert!(!entries.iter().any(|(_, entry)| matches!(entry.record,
            Record::InstructionAttemptBoundarySettled { started_lsn, .. } if started_lsn == failed_started)));
    };
    assert_failed_unresolved();
    let open_count = ptys.opens().len();
    assert_eq!(
        open_count, 3,
        "the failed physical open was the third host call"
    );
    let refused = attach(&mut third, &settings, hired.agent);
    assert!(
        refused.error.is_some(),
        "an unresolved failed physical open refuses another attachment"
    );
    assert_eq!(
        ptys.opens().len(),
        open_count,
        "refusal precedes another native open call"
    );
    assert_failed_unresolved();
    drop(third);
    assert_eq!(
        running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
    assert_eq!(
        ptys.closes(),
        vec![attached.pty, replacement.pty],
        "the failed open creates no owned PTY handle to close"
    );
    for _ in 0..2 {
        let restarted_ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
        assert!(replay_run(&fixture, restarted_ptys.clone()) >= failed_started);
        assert!(restarted_ptys.opens().is_empty());
        assert!(restarted_ptys.closes().is_empty());
        assert_failed_unresolved();
    }
}

// ---- pty.write ------------------------------------------------------------------------------
//
// `pty.write` has one `Apply` cell — operator role, a live connection-bound
// authority, provenance exactly `NoRecordedAgent`, and **this connection owning this `PtyId`** —
// and an applied write does this: fsync the request, call the host once, fsync the
// outcome. The four tests below each distinguish one of those conditions.

/// A daemon, its scripted PTY port, and one connection holding a live attachment.
struct Attached {
    settings: duded::Settings,
    running: common::Running,
    agent: AgentId,
    pty: dudes_proto::rpc::pty::PtyId,
    /// Accepted in the pre-agent epoch and holding the attachment: the only connection that may
    /// write to [`Self::pty`].
    owner: LocalClient,
    /// Also accepted in the pre-agent epoch, so it holds an authority lease and fails the
    /// *ownership* row rather than the authority row. The method's `Requirement` admits it; the pty-write row does not.
    sibling: LocalClient,
    append_failure: duded::runtime::AppendFailure,
    host: SharedHost,
}

/// Boot, hire, confirm a session, and attach — the state every `pty.write` test starts from.
///
/// The order is the production one and it is load-bearing: `owner` and `sibling` connect **before**
/// the first hire, so both hold an `OperatorEffectAuthority` lease, and the hire then seals
/// acquisition for every later connection. A test that connected after the hire would be exercising
/// the authority row whichever other row it meant to exercise.
fn attach_one(fixture: &Fixture, ptys: &SharedPtyHost) -> Attached {
    let Hired {
        settings,
        running,
        agent,
        mut owner,
        sibling,
        append_failure,
        host,
    } = hired(fixture, ptys, fixture.settings());
    let response = attach(&mut owner, &settings, agent);
    assert!(response.error.is_none(), "attach was refused: {response:?}");
    let attached: AttachResult =
        serde_json::from_value(response.result.expect("attach result")).unwrap();
    Attached {
        settings,
        running,
        agent,
        pty: attached.pty,
        owner,
        sibling,
        append_failure,
        host,
    }
}

/// A daemon on `settings` with one hired agent whose session is confirmed, and the two
/// connections accepted before the hire.
struct Hired {
    settings: duded::Settings,
    running: common::Running,
    agent: AgentId,
    owner: LocalClient,
    sibling: LocalClient,
    append_failure: duded::runtime::AppendFailure,
    host: SharedHost,
}

/// Boot on `settings`, hire and confirm a session: everything an attach needs before it is asked.
fn hired(fixture: &Fixture, ptys: &SharedPtyHost, settings: duded::Settings) -> Hired {
    hired_with_resolver(fixture, ptys, settings, common::native_resolver(ptys))
}

fn hired_with_resolver(
    fixture: &Fixture,
    ptys: &SharedPtyHost,
    settings: duded::Settings,
    resolver: duded::classify::Resolver,
) -> Hired {
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let children = duded::ChildEnds::new();
    host.bind_child_ends(Arc::clone(&children));
    let seams = duded::Seams {
        host: host.clone(),
        pty_host: Box::new(ptys.clone()),
        predicates: Arc::new(ScriptedRunner::scripted([common::passes()])),
        clock: Arc::new(TestClock::new()),
        rng: FakeRng::new(),
        resolver,
        adapters: dudes_providers::registry::Adapters::from_config(
            &dudes_providers::registry::ProvidersConfig {
                claude: Some(dudes_providers::claude::config::ClaudeConfig::default()),
            },
            &[],
        )
        .expect("default Claude adapter"),
        children,
        witness: Witness::default(),
    };
    let daemon = duded::boot(&settings, seams).expect("boot");
    let append_failure = daemon.append_failure();
    let mut running = common::start(daemon);

    let mut owner = Client::connect(settings.socket()).expect("the owning connection");
    let mut sibling = Client::connect(settings.socket()).expect("a second pre-agent connection");
    // A barrier, and a necessary one: `connect` returns when the socket is connected, while the
    // authority lease is acquired on the daemon's accept. Without an answered call here the hire
    // below can seal acquisition first and `sibling` starts with no authority — which makes it
    // fail the authority row instead of the ownership row, silently and only sometimes.
    // `system.ping` is `Requirement::AnyRole`, so it takes no authority of its own.
    for (name, client) in [("owner", &mut owner), ("sibling", &mut sibling)] {
        let ping = client
            .call(Method::SystemPing, None)
            .expect("a pre-agent connection is accepted");
        assert!(ping.error.is_none(), "{name} was not accepted: {ping:?}");
    }
    let hire = owner
        .call_as_operator(
            Method::AgentHire,
            Some(serde_json::to_value(common::hire_params(fixture.state.path())).unwrap()),
            credential_of(&settings),
        )
        .expect("hire response");
    assert!(hire.error.is_none(), "hire was refused: {hire:?}");
    let hired: dudes_proto::rpc::agent::HireResult =
        serde_json::from_value(hire.result.expect("hire result")).unwrap();

    let goal = owner
        .call_as_operator(
            Method::GoalSet,
            Some(
                serde_json::to_value(common::goal_params(hired.agent, &["/usr/bin/true"])).unwrap(),
            ),
            credential_of(&settings),
        )
        .expect("goal response");
    assert!(goal.error.is_none(), "goal was refused: {goal:?}");
    confirm_session(fixture, &mut running, &host);
    Hired {
        settings,
        running,
        agent: hired.agent,
        owner,
        sibling,
        append_failure,
        host,
    }
}

/// A hook command the daemon cannot name in the CLI's settings refuses the attach,
/// and the daemon keeps serving. Claude Code treats a hook it cannot run as a non-blocking error,
/// so a CLI launched with one would run ungoverned.
fn assert_an_unusable_hook_command_refuses_the_attach(hook_command: std::path::PathBuf) {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let settings = duded::Settings {
        hook_command: Some(hook_command),
        ..fixture.settings()
    };
    let Hired {
        settings,
        mut running,
        agent,
        mut owner,
        ..
    } = hired(&fixture, &ptys, settings);

    let refused = attach(&mut owner, &settings, agent);
    let ping = owner
        .call(Method::SystemPing, None)
        .expect("the daemon still answers");
    drop(owner);
    assert_eq!(
        running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended,
        "the loop survived the refused attach"
    );

    assert!(
        refused.error.is_some(),
        "the attach was admitted: {refused:?}"
    );
    assert!(ping.error.is_none(), "control: the connection is served");
    assert!(ptys.opens().is_empty(), "no PTY was opened");
    assert_eq!(
        count(&fixture, |record| matches!(record, Record::Attached { .. })),
        0,
        "nothing was attached"
    );
}

#[test]
fn a_missing_hook_command_refuses_the_attach() {
    let missing = tempfile::tempdir()
        .expect("a directory")
        .path()
        .join("dudes-hook");
    assert_an_unusable_hook_command_refuses_the_attach(missing);
}

#[cfg(unix)]
#[test]
fn a_hook_command_that_is_not_utf8_refuses_the_attach() {
    use std::os::unix::ffi::OsStringExt as _;

    assert_an_unusable_hook_command_refuses_the_attach(std::path::PathBuf::from(
        std::ffi::OsString::from_vec(b"/nonexistent/dudes-\xffhook".to_vec()),
    ));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one exact selector proves the durable close order and its owning-connection controls"
)]
fn agent_detach_waits_for_confirmed_close_before_loop_resume() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);

    let no_owner = session
        .sibling
        .call(
            Method::AgentDetach,
            Some(
                serde_json::to_value(AgentRef {
                    agent: session.agent,
                })
                .unwrap(),
            ),
        )
        .expect("unproven response");
    assert!(
        no_owner.error.is_some(),
        "the method's `Requirement` rejects the unproven peer first"
    );
    assert!(ptys.closes().is_empty());

    let sibling = detach(&mut session.sibling, &session.settings, session.agent);
    assert!(
        sibling.error.is_some(),
        "a sibling cannot close the owner's PTY"
    );
    assert!(ptys.closes().is_empty());

    let wrong_agent = detach(
        &mut session.owner,
        &session.settings,
        AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAY").expect("other agent id"),
    );
    assert!(
        wrong_agent.error.is_some(),
        "the owner cannot detach another agent"
    );
    assert!(ptys.closes().is_empty());

    let mut late = Client::connect(session.settings.socket()).expect("post-hire connection");
    let no_authority = detach(&mut late, &session.settings, session.agent);
    assert!(no_authority.error.is_some(), "late authority cannot Apply");
    assert!(ptys.closes().is_empty());

    let malformed = session
        .owner
        .call_as_operator(
            Method::AgentDetach,
            Some(serde_json::json!({ "agent": session.agent, "extra": true })),
            credential_of(&session.settings),
        )
        .expect("malformed response");
    assert_eq!(
        malformed.error.expect("invalid params").code,
        dudes_proto::rpc::ErrorCode::InvalidParams.code()
    );
    assert!(ptys.closes().is_empty());

    let goal = session
        .owner
        .call_as_operator(
            Method::GoalSet,
            Some(
                serde_json::to_value(common::goal_params(session.agent, &["/usr/bin/true"]))
                    .unwrap(),
            ),
            credential_of(&session.settings),
        )
        .expect("standing goal response");
    assert!(goal.error.is_none(), "a goal stands behind attachment");
    let before = fixture.entries().len();
    let accepted_write = write_bytes(
        &mut session.owner,
        &session.settings,
        session.pty,
        b"before-detach",
    );
    assert!(
        accepted_write.error.is_none(),
        "one owned write is accepted"
    );
    let response = detach(&mut session.owner, &session.settings, session.agent);
    assert!(response.error.is_none(), "detach was refused: {response:?}");
    assert_eq!(ptys.closes(), vec![session.pty]);
    assert_eq!(ptys.requested_before_close(), vec![true]);

    let entries = fixture.entries();
    let tail = &entries[before..];
    let (request_at, (request_lsn, _)) = tail
        .iter()
        .enumerate()
        .find(|(_, (_, entry))| {
            matches!(&entry.record,
                Record::PtyCloseRequested {
                    pty,
                    actor: Actor::Peer { .. },
                    provenance: Some(PeerProvenance::NoRecordedAgent),
                    reason,
                } if *pty == session.pty && reason == "agent.detach"
            )
        })
        .expect("peer-attributed request is durable");
    let write_completed_at = tail
        .iter()
        .position(|(_, entry)| {
            matches!(
                &entry.record,
                Record::PtyWriteCompleted {
                    outcome: dudes_proto::PtyWriteOutcome::Complete { written_len: 13 },
                    ..
                }
            )
        })
        .expect("the admitted write completed before close");
    let completed_at = tail
        .iter()
        .position(|(_, entry)| matches!(&entry.record,
            Record::PtyCloseCompleted { request_lsn: seen, outcome: dudes_proto::PtyCloseOutcome::Closed }
            if seen == request_lsn
        ))
        .expect("the host confirmed the close");
    let detached_at = tail
        .iter()
        .position(|(_, entry)| matches!(&entry.record,
            Record::Detached { agent, by: Actor::Peer { .. }, provenance: Some(PeerProvenance::NoRecordedAgent) }
            if *agent == session.agent
        ))
        .expect("the explicit detach follows confirmation");
    let decided_at = tail
        .iter()
        .position(|(_, entry)| {
            matches!(&entry.record,
                Record::LoopDecided { agent, .. } if *agent == session.agent
            )
        })
        .expect("the managed loop re-decides after detach");
    assert!(
        write_completed_at < request_at
            && request_at < completed_at
            && completed_at < detached_at
            && detached_at < decided_at
    );

    let duplicate = detach(&mut session.owner, &session.settings, session.agent);
    assert!(
        duplicate.error.is_some(),
        "the revoked binding cannot detach twice"
    );
    assert_eq!(ptys.closes(), vec![session.pty]);
    let writes_before = ptys.writes();
    let after = write_bytes(
        &mut session.owner,
        &session.settings,
        session.pty,
        b"after-detach",
    );
    assert!(after.error.is_some(), "revoked connection cannot write");
    assert_eq!(ptys.writes(), writes_before);

    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );

    assert_failed_close_is_retryable_without_regrant();
    assert_pre_requested_append_fault_never_closes_before_system_retry();
    assert_final_detached_append_fault_retains_recovery();
    assert_repeated_close_failure_stays_attached_and_revoked();
    assert_failed_final_shutdown_close_never_detaches();
    assert_already_closed_confirms_detach();
    assert_connection_loss_close_is_system_attributed();
    assert_replay_does_not_act_on_orphan_close_request();
    assert_replayed_attachment_stays_held_without_observed_close();
}

fn assert_failed_close_is_retryable_without_regrant() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);
    ptys.fail_next_close();
    let response = detach(&mut session.owner, &session.settings, session.agent);
    assert!(
        response.error.is_some(),
        "an unconfirmed close is not a detach success"
    );
    fixture.await_record("system retry confirmation", |record| {
        matches!(
            record,
            Record::PtyCloseCompleted {
                outcome: dudes_proto::PtyCloseOutcome::Closed,
                ..
            }
        )
        .then_some(())
    });
    fixture.await_record("system retry Detached", |record| {
        matches!(record, Record::Detached {
            agent,
            by: Actor::System,
            provenance: None,
        } if *agent == session.agent)
        .then_some(())
    });
    let records = fixture.records();
    let failure = records
        .iter()
        .position(|record| {
            matches!(
                record,
                Record::PtyCloseCompleted {
                    outcome: dudes_proto::PtyCloseOutcome::Failed { .. },
                    ..
                }
            )
        })
        .expect("the failed host outcome is durable");
    let retry = records
        .iter()
        .position(|record| {
            matches!(
                record,
                Record::PtyCloseRequested {
                    actor: Actor::System,
                    provenance: None,
                    ..
                }
            )
        })
        .expect("a system retry is scheduled without peer authority");
    let closed = records
        .iter()
        .position(|record| {
            matches!(
                record,
                Record::PtyCloseCompleted {
                    outcome: dudes_proto::PtyCloseOutcome::Closed,
                    ..
                }
            )
        })
        .expect("the retry closed the PTY");
    let detached = records
        .iter()
        .position(|record| {
            matches!(record, Record::Detached {
            agent,
            by: Actor::System,
            provenance: None,
        } if *agent == session.agent)
        })
        .expect("only the confirmed retry detaches");
    assert!(failure < retry && retry < closed && closed < detached);
    assert_eq!(ptys.closes(), vec![session.pty, session.pty]);
    assert_eq!(ptys.requested_before_close(), vec![true, true]);
    let writes_before = ptys.writes();
    let refused = write_bytes(
        &mut session.owner,
        &session.settings,
        session.pty,
        b"revoked",
    );
    assert!(refused.error.is_some());
    assert_eq!(ptys.writes(), writes_before);
    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

fn assert_pre_requested_append_fault_never_closes_before_system_retry() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);
    let before = fixture.entries().len();
    session
        .append_failure
        .fail_nth(NonZeroUsize::new(1).expect("first append"));
    let refused = detach(&mut session.owner, &session.settings, session.agent);
    assert!(refused.error.is_some(), "the request append failed");
    fixture.await_record("system retry Detached", |record| {
        matches!(record, Record::Detached {
            agent,
            by: Actor::System,
            provenance: None,
        } if *agent == session.agent)
        .then_some(())
    });
    let entries = fixture.entries();
    let tail = &entries[before..];
    assert!(
        !tail.iter().any(|(_, entry)| matches!(
            &entry.record,
            Record::PtyCloseRequested {
                actor: Actor::Peer { .. },
                ..
            }
        )),
        "the failed peer request never became durable"
    );
    let system_request = tail
        .iter()
        .position(|(_, entry)| {
            matches!(&entry.record,
                Record::PtyCloseRequested {
                    pty,
                    actor: Actor::System,
                    provenance: None,
                    ..
                } if *pty == session.pty
            )
        })
        .expect("a distinct system retry was journaled");
    let completion = tail
        .iter()
        .position(|(_, entry)| {
            matches!(
                &entry.record,
                Record::PtyCloseCompleted {
                    outcome: dudes_proto::PtyCloseOutcome::Closed,
                    ..
                }
            )
        })
        .expect("the system retry completed");
    let detached = tail
        .iter()
        .position(|(_, entry)| {
            matches!(&entry.record,
                Record::Detached { agent, by: Actor::System, provenance: None }
                if *agent == session.agent
            )
        })
        .expect("the system retry detached");
    assert!(system_request < completion && completion < detached);
    assert_eq!(
        ptys.closes(),
        vec![session.pty],
        "only the durable retry called the host"
    );
    assert_eq!(ptys.requested_before_close(), vec![true]);
    let refused_write = write_bytes(
        &mut session.owner,
        &session.settings,
        session.pty,
        b"revoked",
    );
    assert!(
        refused_write.error.is_some(),
        "the peer binding was not restored"
    );
    assert!(ptys.writes().is_empty());
    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

fn assert_final_detached_append_fault_retains_recovery() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);
    let goal = session
        .owner
        .call_as_operator(
            Method::GoalSet,
            Some(
                serde_json::to_value(common::goal_params(session.agent, &["/usr/bin/true"]))
                    .unwrap(),
            ),
            credential_of(&session.settings),
        )
        .expect("standing goal");
    assert!(goal.error.is_none());
    session.running.handle.doctor().expect("goal has settled");
    let before = fixture.entries().len();
    session
        .append_failure
        .fail_nth(NonZeroUsize::new(5).expect("Detached append"));
    let refused = detach(&mut session.owner, &session.settings, session.agent);
    assert!(refused.error.is_some(), "the final lifecycle append failed");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !fixture.records().iter().any(|record| {
        matches!(record,
            Record::Detached { agent, by: Actor::System, provenance: None }
            if *agent == session.agent
        )
    }) {
        assert!(
            std::time::Instant::now() < deadline,
            "confirmed close stranded Attached after its first Detached append failed"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let entries = fixture.entries();
    let tail = &entries[before..];
    let completed = tail
        .iter()
        .position(|(_, entry)| {
            matches!(
                &entry.record,
                Record::PtyCloseCompleted {
                    outcome: dudes_proto::PtyCloseOutcome::Closed,
                    ..
                }
            )
        })
        .expect("host confirmation survived the fault");
    let detached = tail
        .iter()
        .position(|(_, entry)| {
            matches!(&entry.record,
                Record::Detached { agent, by: Actor::System, provenance: None }
                if *agent == session.agent
            )
        })
        .expect("system retry made Detached durable");
    assert!(completed < detached);
    assert_eq!(
        tail.iter()
            .filter(|(_, entry)| matches!(&entry.record,
                Record::Detached { agent, .. } if *agent == session.agent
            ))
            .count(),
        1,
        "one confirmed close yields one lifecycle transition"
    );
    assert_eq!(
        ptys.closes(),
        vec![session.pty],
        "the retry must not re-close a confirmed PTY"
    );
    let refused_write = write_bytes(
        &mut session.owner,
        &session.settings,
        session.pty,
        b"revoked",
    );
    assert!(refused_write.error.is_some());
    assert!(ptys.writes().is_empty());
    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

fn assert_failed_final_shutdown_close_never_detaches() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);
    ptys.fail_next_closes(3);
    ptys.fail_next_shutdown();
    let response = detach(&mut session.owner, &session.settings, session.agent);
    assert!(response.error.is_some());
    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
    let records = fixture.records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                Record::PtyCloseCompleted {
                    outcome: dudes_proto::PtyCloseOutcome::Failed { .. },
                    ..
                }
            ))
            .count(),
        4,
        "explicit, queued retry, per-handle shutdown and aggregate shutdown remain visible"
    );
    assert_eq!(
        records
            .iter()
            .filter(
                |record| matches!(record, Record::Detached { agent, .. } if *agent == session.agent)
            )
            .count(),
        0,
        "a failed final host shutdown cannot pretend the PTY was closed"
    );
    assert_eq!(ptys.closes(), vec![session.pty, session.pty, session.pty]);
    assert_eq!(ptys.requested_before_close(), vec![true, true, true]);
    assert_eq!(ptys.shutdowns(), 1, "the invariant-10 backstop still ran");
    assert_eq!(
        ptys.requested_before_shutdown(),
        vec![true],
        "the aggregate host call was preceded by this PTY's fourth durable request"
    );
}

fn assert_repeated_close_failure_stays_attached_and_revoked() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);
    let goal = session
        .owner
        .call_as_operator(
            Method::GoalSet,
            Some(
                serde_json::to_value(common::goal_params(session.agent, &["/usr/bin/true"]))
                    .unwrap(),
            ),
            credential_of(&session.settings),
        )
        .expect("standing goal");
    assert!(goal.error.is_none());
    let before = fixture.records().len();
    ptys.fail_next_closes(2);
    let response = detach(&mut session.owner, &session.settings, session.agent);
    assert!(response.error.is_some());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while count(&fixture, |record| {
        matches!(
            record,
            Record::PtyCloseCompleted {
                outcome: dudes_proto::PtyCloseOutcome::Failed { .. },
                ..
            }
        )
    }) < 2
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the system retry was recorded"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let records = fixture.records();
    let tail = &records[before..];
    assert!(
        !tail.iter().any(
            |record| matches!(record, Record::Detached { agent, .. } if *agent == session.agent)
        ),
        "no failed close may detach"
    );
    assert!(
        !tail.iter().any(
            |record| matches!(record, Record::LoopDecided { agent, .. } if *agent == session.agent)
        ),
        "no failed close may re-decide the standing goal"
    );
    assert_eq!(ptys.closes(), vec![session.pty, session.pty]);
    let writes_before = ptys.writes();
    let refused = write_bytes(
        &mut session.owner,
        &session.settings,
        session.pty,
        b"revoked",
    );
    assert!(refused.error.is_some());
    assert_eq!(ptys.writes(), writes_before);
    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
    let records = fixture.records();
    let shutdown_completed = records
        .iter()
        .rposition(|record| {
            matches!(
                record,
                Record::PtyCloseCompleted {
                    outcome: dudes_proto::PtyCloseOutcome::Closed,
                    ..
                }
            )
        })
        .expect("shutdown retry confirmed the retained PTY absent");
    let shutdown_detached = records
        .iter()
        .rposition(|record| {
            matches!(record, Record::Detached {
            agent,
            by: Actor::System,
            provenance: None,
        } if *agent == session.agent)
        })
        .expect("shutdown recorded one confirmed system detach");
    assert!(shutdown_completed < shutdown_detached);
    assert_eq!(
        records
            .iter()
            .filter(
                |record| matches!(record, Record::Detached { agent, .. } if *agent == session.agent)
            )
            .count(),
        1,
        "the retained handle is detached once"
    );
    assert_eq!(ptys.closes(), vec![session.pty, session.pty, session.pty]);
    assert_eq!(ptys.requested_before_close(), vec![true, true, true]);
}

fn assert_already_closed_confirms_detach() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);
    ptys.mark_already_closed(session.pty);
    let response = detach(&mut session.owner, &session.settings, session.agent);
    assert!(
        response.error.is_none(),
        "AlreadyClosed confirms absence: {response:?}"
    );
    let records = fixture.records();
    let completed = records
        .iter()
        .position(|record| {
            matches!(
                record,
                Record::PtyCloseCompleted {
                    outcome: dudes_proto::PtyCloseOutcome::AlreadyClosed,
                    ..
                }
            )
        })
        .expect("AlreadyClosed was journaled");
    let detached = records
        .iter()
        .position(|record| {
            matches!(record, Record::Detached {
            agent,
            by: Actor::Peer { .. },
            provenance: Some(PeerProvenance::NoRecordedAgent),
        } if *agent == session.agent)
        })
        .expect("peer detach followed confirmation");
    assert!(completed < detached);
    assert_eq!(ptys.closes(), vec![session.pty]);
    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

fn assert_connection_loss_close_is_system_attributed() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);
    let agent = session.agent;
    let pty = session.pty;
    drop(session.owner);
    fixture.await_record("connection-loss system detach", |record| {
        matches!(record, Record::Detached {
            agent: seen,
            by: Actor::System,
            provenance: None,
        } if *seen == agent)
        .then_some(())
    });
    let records = fixture.records();
    let requested = records
        .iter()
        .position(|record| {
            matches!(record, Record::PtyCloseRequested {
            pty: seen,
            actor: Actor::System,
            provenance: None,
            ..
        } if *seen == pty)
        })
        .expect("the system close request is durable");
    let completed = records
        .iter()
        .position(|record| {
            matches!(
                record,
                Record::PtyCloseCompleted {
                    outcome: dudes_proto::PtyCloseOutcome::Closed,
                    ..
                }
            )
        })
        .expect("the system close completed");
    let detached = records
        .iter()
        .position(|record| {
            matches!(record, Record::Detached {
            agent: seen,
            by: Actor::System,
            provenance: None,
        } if *seen == agent)
        })
        .expect("the system detached only after confirmation");
    assert!(requested < completed && completed < detached);
    assert_eq!(ptys.closes(), vec![pty]);
    assert_eq!(ptys.requested_before_close(), vec![true]);
    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

#[test]
fn resize_and_output_reach_only_the_live_attachment_and_journal_nothing() {
    use duded::ports::{Answered, Ask};
    use duded::pty_output::Output;
    use dudes_proto::rpc::ErrorCode;
    use dudes_proto::rpc::pty::PtyId;

    const NOW: std::time::Duration = std::time::Duration::ZERO;
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);
    let (agent, pty) = (session.agent, session.pty);
    let stranger = AgentId::from_parts(1, 1);
    let handle = session.running.handle.clone();
    let resize = |agent, pty, cols, rows| {
        handle.ask(Ask::PtyResize {
            agent,
            pty,
            cols,
            rows,
        })
    };
    let journaled = fixture.records().len();

    ptys.produce_output(pty, b"before a subscriber ");
    let output = handle
        .subscribe_output(agent, pty)
        .expect("the live attachment's output");
    ptys.produce_output(pty, b"after");
    assert_eq!(
        output.next(NOW),
        Output::Data(b"before a subscriber after".to_vec())
    );
    assert_eq!(output.next(NOW), Output::Idle);
    for (who, handle_id, why) in [
        (agent, pty, "a second subscriber"),
        (agent, PtyId(pty.0 + 1), "another handle"),
        (stranger, pty, "another agent"),
    ] {
        let refusal = handle.subscribe_output(who, handle_id).expect_err(why);
        assert_eq!(refusal.code, ErrorCode::Refused.code(), "{why}");
    }

    for (cols, rows) in [(0, 40), (120, 0), (32_768, 40), (120, 32_768)] {
        let refusal = resize(agent, pty, cols, rows).expect_err("an out-of-range size");
        assert_eq!(refusal.code, ErrorCode::InvalidParams.code());
    }
    for (who, handle_id) in [(agent, PtyId(pty.0 + 1)), (stranger, pty)] {
        let refusal = resize(who, handle_id, 120, 40).expect_err("an unbound resize");
        assert_eq!(refusal.code, ErrorCode::Refused.code());
    }
    assert!(
        ptys.resizes().is_empty(),
        "a refused resize reached the host"
    );
    assert!(matches!(resize(agent, pty, 120, 40), Ok(Answered::Done)));
    assert_eq!(ptys.resizes(), vec![(pty, 120, 40)]);
    ptys.fail_next_resize();
    let refusal = resize(agent, pty, 90, 30).expect_err("a failed host resize");
    assert_eq!(
        (refusal.code, refusal.message.as_str()),
        (ErrorCode::Refused.code(), "the PTY could not be resized"),
        "a host failure is refused without the host's own text"
    );
    assert_eq!(ptys.resizes(), vec![(pty, 120, 40), (pty, 90, 30)]);
    assert_eq!(
        fixture.records().len(),
        journaled,
        "output and resize journal nothing"
    );

    drop(session.owner);
    fixture.await_record("connection-loss system detach", |record| {
        matches!(record, Record::Detached { agent: seen, by: Actor::System, .. } if *seen == agent)
            .then_some(())
    });
    assert_eq!(
        output.next(NOW),
        Output::Ended,
        "the closed PTY's output ended"
    );
    assert!(resize(agent, pty, 100, 30).is_err());
    assert!(handle.subscribe_output(agent, pty).is_err());
    assert_eq!(ptys.resizes(), vec![(pty, 120, 40), (pty, 90, 30)]);
    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

#[test]
fn pty_write_refuses_an_actor_and_provenance_that_do_not_pair() {
    use duded::ports::Ask;
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);
    let requested = || {
        count(&fixture, |record| {
            matches!(record, Record::PtyWriteRequested { .. })
        })
    };
    let before = requested();
    for actor in [
        Actor::System,
        Actor::Agent {
            agent: session.agent,
        },
        Actor::BrowserSession {
            session_digest: dudes_proto::record::Hex32::new([7; 32]),
        },
    ] {
        let refusal = session.running.handle.ask(Ask::PtyWrite {
            pty: session.pty,
            bytes: b"x".to_vec(),
            payload: dudes_proto::PayloadDigest {
                decoded_len: 1,
                blake3: blake3::hash(b"x").to_hex().to_string(),
            },
            actor,
            provenance: PeerProvenance::NoRecordedAgent,
        });
        assert!(refusal.is_err(), "{actor:?} with a provenance was written");
    }
    assert_eq!(requested(), before, "a mismatched pair reached the journal");
    assert!(
        ptys.writes().is_empty(),
        "a mismatched pair reached the host"
    );
    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

fn assert_replay_does_not_act_on_orphan_close_request() {
    let fixture = Fixture::new();
    let orphan = dudes_proto::rpc::pty::PtyId(999);
    seed_orphan_close_request(&fixture, orphan);
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let last = replay_run(&fixture, ptys.clone());
    assert!(
        last > dudes_types::Lsn::ZERO,
        "the orphan request was replayed"
    );
    assert!(
        ptys.closes().is_empty(),
        "replay never retries a close request"
    );
    assert_eq!(
        count(&fixture, |record| matches!(record, Record::Detached { .. })),
        0,
        "an unmatched close request alone cannot resume an agent"
    );
}

fn assert_replayed_attachment_stays_held_without_observed_close() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);
    let agent = session.agent;
    let goal = session
        .owner
        .call_as_operator(
            Method::GoalSet,
            Some(serde_json::to_value(common::goal_params(agent, &["/usr/bin/true"])).unwrap()),
            credential_of(&session.settings),
        )
        .expect("standing goal");
    assert!(goal.error.is_none());
    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
    let attached_lsn = seed_replayed_attached(&fixture, agent);
    let before = fixture.entries().len();
    let replayed = SharedPtyHost::new(fixture.journal_dir());
    let reached = replay_run(&fixture, replayed.clone());
    assert!(
        reached >= attached_lsn,
        "the new daemon replayed the seeded Attached"
    );
    let entries = fixture.entries();
    let tail = &entries[before..];
    assert!(
        !tail.iter().any(|(_, entry)| matches!(&entry.record,
            Record::Detached { agent: seen, .. }
            | Record::LoopDecided { agent: seen, .. }
            | Record::TurnIssued { agent: seen, .. }
            if *seen == agent
        )),
        "replayed Attached without observed close must not detach, decide or spawn: {tail:?}"
    );
    assert!(replayed.opens().is_empty() && replayed.closes().is_empty());
    assert_eq!(replayed.shutdowns(), 1, "the replay control used this host");
}

/// Drive one turn to a vendor-confirmed session, which is attach's precondition.
fn confirm_session(fixture: &Fixture, running: &mut common::Running, host: &SharedHost) {
    let (turn, session) = fixture.await_record("TurnIssued", |record| match record {
        Record::TurnIssued {
            turn,
            session: Some(session),
            ..
        } => Some((*turn, session.clone())),
        _ => None,
    });
    running
        .handle
        .input(Input::SessionLearned { turn, session });
    fixture.await_record("SessionLearned", |record| match record {
        Record::SessionLearned { turn: seen, .. } if *seen == turn => Some(()),
        _ => None,
    });
    common::end_turn_with_native(fixture, &running.handle, host, turn);
    fixture.await_record("TurnEnded", |record| match record {
        Record::TurnEnded { turn: seen, .. } if *seen == turn => Some(()),
        _ => None,
    });
    fixture.await_record("PredicateResult", |record| {
        matches!(record, Record::PredicateResult { .. }).then_some(())
    });
}

/// `pty.write` as an operator, with `data` already encoded.
fn write_encoded(
    client: &mut LocalClient,
    settings: &duded::Settings,
    pty: dudes_proto::rpc::pty::PtyId,
    data: &str,
) -> dudes_proto::rpc::Response {
    client
        .call_as_operator(
            Method::PtyWrite,
            Some(serde_json::json!({ "pty": pty, "data": data })),
            credential_of(settings),
        )
        .expect("pty.write response")
}

/// `pty.write` as an operator, encoding `bytes` canonically first.
fn write_bytes(
    client: &mut LocalClient,
    settings: &duded::Settings,
    pty: dudes_proto::rpc::pty::PtyId,
    bytes: &[u8],
) -> dudes_proto::rpc::Response {
    write_encoded(client, settings, pty, &common::base64_standard(bytes))
}

/// How many records of one shape the journal holds.
fn count(fixture: &Fixture, predicate: impl Fn(&Record) -> bool) -> usize {
    fixture.records().iter().filter(|r| predicate(r)).count()
}

/// The reason on every `pty.write` suppression, in order.
fn suppressions(fixture: &Fixture) -> Vec<String> {
    fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::ConditionedEffectSuppressed {
                method: dudes_proto::ConditionedMethod::PtyWrite,
                reason,
                ..
            } => Some(reason),
            _ => None,
        })
        .collect()
}

/// Every `PtyWriteRequested`, paired with the position **this journal** gave it.
fn requests(fixture: &Fixture) -> Vec<(dudes_types::Lsn, Record)> {
    fixture
        .entries()
        .into_iter()
        .filter(|(_, entry)| matches!(entry.record, Record::PtyWriteRequested { .. }))
        .map(|(lsn, entry)| (lsn, entry.record))
        .collect()
}

/// The first distinction: a `Requirement` refusal is *before* the conditioned route.
///
/// The two negative paths look identical from outside — both are an error and no bytes — and they
/// are not the same thing. A peer that fails `Requirement::Operator` never reaches
/// `Route::Conditioned` at all, so it leaves **no** `ConditionedEffectSuppressed`; a peer that
/// passes its `Requirement` and fails the effect-local table leaves exactly one. A test that only counted
/// bytes would pass with the route wired the wrong way round.
///
/// The control runs first and is an applied write, so the assertions below are about the refusal
/// rather than about a daemon in which nothing can reach the host.
#[test]
fn pty_write_requirement_refusal_has_no_conditioned_disposition() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);

    // Control, before the subject: the owning operator's bytes reach the host.
    let applied = write_bytes(&mut session.owner, &session.settings, session.pty, b"ok");
    assert!(applied.error.is_none(), "the control write: {applied:?}");
    assert_eq!(ptys.writes(), vec![(session.pty, b"ok".to_vec())]);
    let dispositions = suppressions(&fixture);
    assert!(
        dispositions.is_empty(),
        "no suppression yet: {dispositions:?}"
    );
    let requested = count(&fixture, |record| {
        matches!(record, Record::PtyWriteRequested { .. })
    });
    assert_eq!(requested, 1);

    // Subject: a peer that presents no operator credential resolves to `Unproven`, which the method's `Requirement`
    // refuses before the route is invoked.
    let mut unproven = Client::connect(session.settings.socket()).expect("an unproven connection");
    let refused = unproven
        .call(
            Method::PtyWrite,
            Some(serde_json::json!({
                "pty": session.pty,
                "data": common::base64_standard(b"never"),
            })),
        )
        .expect("a response");
    let error = refused.error.expect("the call was refused");
    assert_eq!(
        error.code,
        dudes_proto::rpc::ErrorCode::Unauthorized.code(),
        "a `Requirement` refusal is Unauthorized, not the conditioned route's Refused: {error:?}"
    );

    assert_eq!(
        suppressions(&fixture),
        Vec::<String>::new(),
        "a call refused before the route appends no disposition"
    );
    assert_eq!(
        count(&fixture, |record| matches!(
            record,
            Record::PtyWriteRequested { .. }
        )),
        requested,
        "and no request record"
    );
    assert_eq!(
        ptys.writes(),
        vec![(session.pty, b"ok".to_vec())],
        "the host saw only the control write"
    );

    drop(unproven);
    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

/// The second distinction: an admitted call that the table refuses suppresses **once**.
///
/// Four cells reach `Suppress` and one does not reach the table at all. Each is asserted for its
/// own reason string, because "a suppression happened" is satisfied by the wrong row as easily as
/// by the right one, and each is asserted to add exactly one record and no host bytes.
#[test]
fn pty_write_conditioned_refusal_suppresses_once_without_sink() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);

    // Control, before the subjects.
    let applied = write_bytes(&mut session.owner, &session.settings, session.pty, b"ok");
    assert!(applied.error.is_none(), "the control write: {applied:?}");
    let host_after_control = ptys.writes();
    assert_eq!(host_after_control, vec![(session.pty, b"ok".to_vec())]);

    let refused_with = |response: &dudes_proto::rpc::Response| {
        let error = response.error.as_ref().expect("the call was refused");
        assert_eq!(error.code, dudes_proto::rpc::ErrorCode::Refused.code());
        let data: dudes_proto::rpc::RefusedData =
            serde_json::from_value(error.data.clone().expect("refusal data")).unwrap();
        assert_eq!(data.gate, dudes_proto::rpc::RefusalGate::ConditionedEffect);
    };

    // A sibling connection with its own live authority, refused for the one row it fails.
    let unowned = write_bytes(
        &mut session.sibling,
        &session.settings,
        session.pty,
        b"sibling",
    );
    refused_with(&unowned);

    // The owner, over the decoded-byte ceiling.
    let over_cap = vec![b'x'; dudes_rpc::MAX_PTY_WRITE_BYTES + 1];
    let too_big = write_bytes(
        &mut session.owner,
        &session.settings,
        session.pty,
        &over_cap,
    );
    refused_with(&too_big);

    // A connection accepted after the hire sealed acquisition: no authority to begin with.
    let mut late = Client::connect(session.settings.socket()).expect("a post-hire connection");
    let sealed = write_bytes(&mut late, &session.settings, session.pty, b"late");
    refused_with(&sealed);

    // A `PtyId` nobody owns, from the owning connection: still the ownership row, because owning
    // *an* attachment is not owning *this* one.
    let stranger = write_bytes(
        &mut session.owner,
        &session.settings,
        dudes_proto::rpc::pty::PtyId(session.pty.0 + 7),
        b"stranger",
    );
    refused_with(&stranger);

    assert_eq!(
        suppressions(&fixture),
        vec![
            "connection does not own the PTY attachment".to_owned(),
            "decoded payload exceeds the method ceiling".to_owned(),
            "operator authority unavailable".to_owned(),
            "connection does not own the PTY attachment".to_owned(),
        ],
        "each admitted refusal appended exactly one disposition, naming its own row"
    );

    // The third path: invalid params stop before any disposition exists.
    let dispositions = suppressions(&fixture).len();
    let malformed = write_encoded(&mut session.owner, &session.settings, session.pty, "aGk");
    let error = malformed.error.expect("non-canonical base64 is refused");
    assert_eq!(
        error.code,
        dudes_proto::rpc::ErrorCode::InvalidParams.code()
    );
    assert_eq!(
        suppressions(&fixture).len(),
        dispositions,
        "invalid params appended no disposition"
    );

    assert_eq!(
        ptys.writes(),
        host_after_control,
        "not one refused call reached the host"
    );
    assert_eq!(
        count(&fixture, |record| matches!(
            record,
            Record::PtyWriteRequested { .. }
        )),
        1,
        "and only the control was ever requested"
    );

    drop(late);
    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

/// The positive attribution: the record's actor and provenance come from the connection.
///
/// **One of these two is falsifiable by a cell and the other is falsifiable by the params type,
/// and saying which is which is the point.** `actor` has a wrong value available — any other
/// `Actor` variant — so a mutation that built one goes red on the first assertion. `provenance`
/// does not: the `Apply` row admits exactly `NoRecordedAgent`, so a builder that
/// hard-coded that value is indistinguishable here and stays green. What is falsifiable about
/// `provenance` is that no caller can *supply* it, which is the `deny_unknown_fields` arm below.
#[test]
fn pty_write_requested_uses_router_actor_and_exact_peer_provenance() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);

    let bytes = b"\x1b[A\x00\xff".to_vec();
    let applied = write_bytes(&mut session.owner, &session.settings, session.pty, &bytes);
    assert!(applied.error.is_none(), "the write: {applied:?}");

    let requested = requests(&fixture);
    assert_eq!(requested.len(), 1);
    let Record::PtyWriteRequested {
        pty,
        actor,
        provenance,
        payload,
    } = &requested[0].1
    else {
        unreachable!("filtered above")
    };
    assert_eq!(*pty, session.pty);
    assert_eq!(*provenance, Some(PeerProvenance::NoRecordedAgent));
    // The peer on this connection *is* this test process, so the exact pid is assertable — which
    // is what makes this a claim about where the actor came from rather than about its shape.
    let Actor::Peer { pid, uid } = actor else {
        panic!("the actor must be the connected peer, not {actor:?}");
    };
    assert_eq!(
        *pid,
        std::process::id(),
        "the pid is the transport's view of this very process, not anything params carried"
    );
    assert!(
        uid.is_some(),
        "and the peer credential's uid travelled with it"
    );
    assert_eq!(payload.decoded_len, bytes.len() as u64);
    assert_eq!(payload.blake3, blake3::hash(&bytes).to_hex().to_string());
    assert_eq!(ptys.writes(), vec![(session.pty, bytes.clone())]);

    // No params key can carry either fact: `PtyWriteParams` denies unknown fields, so an attempt
    // is `-32602` before the route, with no disposition and no bytes.
    for key in ["actor", "provenance", "by", "pid"] {
        let mut object = serde_json::Map::new();
        object.insert("pty".into(), serde_json::json!(session.pty));
        object.insert(
            "data".into(),
            serde_json::json!(common::base64_standard(b"x")),
        );
        object.insert(key.to_owned(), serde_json::json!("forged"));
        let forged = session
            .owner
            .call_as_operator(
                Method::PtyWrite,
                Some(serde_json::Value::Object(object)),
                credential_of(&session.settings),
            )
            .expect("a response");
        let error = forged
            .error
            .unwrap_or_else(|| panic!("`{key}` in params must be refused"));
        assert_eq!(
            error.code,
            dudes_proto::rpc::ErrorCode::InvalidParams.code()
        );
    }
    assert!(suppressions(&fixture).is_empty());
    assert_eq!(
        ptys.writes(),
        vec![(session.pty, bytes)],
        "no forged call reached the host"
    );

    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

/// The acceptance test: ownership, cross-connection, refusal, suppression, crash/replay
/// and message bodies, on one daemon and one journal.
///
/// # Why these six arms are one test and not six
///
/// Five of them are assertions about **the same host's write log**, and that log is only evidence
/// while every arm shares it. Split apart, each arm would assert "zero writes" against a host that
/// had never written anything at all, which is satisfied by a daemon with the route deleted. Here
/// the first arm puts exactly one entry in that log and every later arm asserts the log is still
/// exactly that — an equality, never a `contains`, so no arm can be satisfied by the bytes it
/// planted itself.
///
/// The three narrower tests above are the sharp checks for single propositions; this one is
/// the round trip they cannot be, because ownership granted in arm 1 is what arm 2 must fail
/// against and arm 6 must not resurrect.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the six arms share one host write log across three daemon runs, which is what makes \
              each arm's assertion evidence rather than a claim about an unused object"
)]
fn pty_write_requires_connection_ownership_and_never_replays_bytes() {
    const OWNED: &[u8] = b"owner-keystrokes";
    const REFUSED: &[u8] = b"refused-keystrokes";
    const PLANTED: &str = "planted-message-body";

    let fixture = Fixture::new();
    // A mail body that is durable *before the daemon ever starts*, so the strongest form of arm 5
    // is covered: mail the daemon replays, rather than mail a test handed it.
    seed_mail(&fixture, PLANTED);

    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);

    // ---- arm 1: ownership, and the order --------------------------------------------
    let applied = write_bytes(&mut session.owner, &session.settings, session.pty, OWNED);
    assert!(applied.error.is_none(), "the owning write: {applied:?}");
    assert_eq!(
        ptys.writes(),
        vec![(session.pty, OWNED.to_vec())],
        "the host received the exact caller-owned bytes, once"
    );
    assert_eq!(
        ptys.requested_before_write(),
        vec![true],
        "PtyWriteRequested was durable before the host was called"
    );

    let requested = requests(&fixture);
    assert_eq!(requested.len(), 1);
    let (request_lsn, _) = requested[0].clone();
    let completions: Vec<Record> = fixture
        .records()
        .into_iter()
        .filter(|record| matches!(record, Record::PtyWriteCompleted { .. }))
        .collect();
    assert_eq!(completions.len(), 1);
    assert!(
        matches!(
            &completions[0],
            Record::PtyWriteCompleted {
                request_lsn: named,
                outcome: dudes_proto::PtyWriteOutcome::Complete { written_len },
            } if *named == request_lsn && *written_len == OWNED.len() as u64
        ),
        "the completion names this journal's own position for the request: {:?} against {request_lsn:?}",
        completions[0]
    );

    // ---- arm 2: another connection cannot use this handle -------------------------------------
    let unowned = write_bytes(
        &mut session.sibling,
        &session.settings,
        session.pty,
        b"sibling-keystrokes",
    );
    assert!(unowned.error.is_some(), "a sibling connection cannot write");
    assert_eq!(ptys.writes(), vec![(session.pty, OWNED.to_vec())]);

    // The sibling arm alone cannot tell "owns *this* handle" from "owns *a* handle" — the sibling
    // owns neither, so both readings refuse it. Measured: narrowing the ownership fact back to
    // "holds any attachment" leaves the arm above green. This is the arm that separates them, and
    // it is the one that matters, because the connection here *is* an attacher.
    let stranger = write_bytes(
        &mut session.owner,
        &session.settings,
        dudes_proto::rpc::pty::PtyId(session.pty.0 + 7),
        b"stranger-keystrokes",
    );
    assert!(
        stranger.error.is_some(),
        "an attached connection cannot write to a handle it did not open"
    );
    assert_eq!(ptys.writes(), vec![(session.pty, OWNED.to_vec())]);

    // ---- arm 3: a `Requirement` refusal, which is before the route ----------------------------------
    let mut unproven = Client::connect(session.settings.socket()).expect("an unproven connection");
    let refused = unproven
        .call(
            Method::PtyWrite,
            Some(serde_json::json!({
                "pty": session.pty,
                "data": common::base64_standard(b"unproven-keystrokes"),
            })),
        )
        .expect("a response");
    assert_eq!(
        refused.error.expect("refused").code,
        dudes_proto::rpc::ErrorCode::Unauthorized.code()
    );
    assert_eq!(
        suppressions(&fixture),
        vec![
            "connection does not own the PTY attachment".to_owned(),
            "connection does not own the PTY attachment".to_owned(),
        ],
        "arm 2's two cells suppressed once each, and arm 3 not at all"
    );
    assert_eq!(ptys.writes(), vec![(session.pty, OWNED.to_vec())]);

    // ---- arm 4: an admitted call the table refuses --------------------------------------------
    let over_cap = vec![b'x'; dudes_rpc::MAX_PTY_WRITE_BYTES + 1];
    let too_big = write_bytes(
        &mut session.owner,
        &session.settings,
        session.pty,
        &over_cap,
    );
    assert!(too_big.error.is_some(), "an over-cap write is refused");
    assert_eq!(
        suppressions(&fixture).len(),
        3,
        "exactly one more disposition"
    );
    assert_eq!(ptys.writes(), vec![(session.pty, OWNED.to_vec())]);

    // ---- arm 5: invariant 6 — no message body becomes a keystroke -----------------------------
    //
    // Every surface that carries a body is planted with the same distinctive text, and each plant
    // is then shown to have *landed* — a plant the daemon refused would prove nothing about what
    // reaches a PTY. The mail arm was seeded above and is replayed rather than sent.
    //
    // An answer lands only on an open question, so one is opened first — through the
    // loop's own command, with the planted text, which makes the question a planted body too.
    session
        .running
        .handle
        .ask(duded::ports::Ask::Ask {
            agent: session.agent,
            text: PLANTED.to_owned(),
        })
        .expect("the question the answer plant closes");
    for (method, params) in [
        (
            Method::AgentSteer,
            serde_json::json!({ "agent": session.agent, "text": PLANTED }),
        ),
        (
            Method::AgentAnswer,
            serde_json::json!({ "agent": session.agent, "text": PLANTED }),
        ),
        (
            Method::MemoryAppend,
            serde_json::json!({ "agent": session.agent, "text": PLANTED }),
        ),
    ] {
        let response = session
            .owner
            .call_as_operator(method, Some(params), credential_of(&session.settings))
            .expect("a response");
        assert!(
            response.error.is_none(),
            "the {} plant must land to be worth anything: {response:?}",
            method.name()
        );
    }
    let replanted_goal = dudes_proto::rpc::goal::GoalSetParams {
        text: PLANTED.to_owned(),
        ..common::goal_params(session.agent, &["/usr/bin/true"])
    };
    let goal = session
        .owner
        .call_as_operator(
            Method::GoalSet,
            Some(serde_json::to_value(replanted_goal).unwrap()),
            credential_of(&session.settings),
        )
        .expect("a response");
    assert!(goal.error.is_none(), "the task plant: {goal:?}");

    let planted_records: Vec<&'static str> = fixture
        .records()
        .iter()
        .filter_map(|record| match record {
            Record::Steer { text, .. } if text == PLANTED => Some("Steer"),
            Record::HumanAnswer { text, .. } if text == PLANTED => Some("HumanAnswer"),
            Record::MemoryAppend { text, .. } if text == PLANTED => Some("MemoryAppend"),
            Record::GoalSet { goal, .. } if goal.text == PLANTED => Some("GoalSet"),
            Record::Mail {
                body: Some(body), ..
            } if body == PLANTED => Some("Mail"),
            _ => None,
        })
        .collect();
    for expected in ["Steer", "HumanAnswer", "MemoryAppend", "GoalSet", "Mail"] {
        assert!(
            planted_records.contains(&expected),
            "the {expected} plant never reached the journal, so its arm proves nothing: \
             {planted_records:?}"
        );
    }
    assert_eq!(
        ptys.writes(),
        vec![(session.pty, OWNED.to_vec())],
        "five message bodies are durable in this journal and none of them reached the PTY"
    );

    // ---- arm 6a: a partial host write revokes this connection's ownership ----------------------
    //
    // Captured before the write, not after: revocation queues the teardown and two RPC round trips
    // follow, so a baseline taken afterwards has already counted the `Detached` it means to wait
    // for and would wait for a second one that never comes.
    let detached_before = count(&fixture, |record| matches!(record, Record::Detached { .. }));
    ptys.short_next_write(3);
    let partial = write_bytes(
        &mut session.owner,
        &session.settings,
        session.pty,
        b"partial-keystrokes",
    );
    assert!(
        partial.error.is_some(),
        "a partial write is not reported as success"
    );
    assert!(
        fixture.records().iter().any(|record| matches!(
            record,
            Record::PtyWriteCompleted {
                outcome: dudes_proto::PtyWriteOutcome::Partial { written_len: 3, .. },
                ..
            }
        )),
        "the host's short count is journaled as what it was"
    );
    let after_partial = ptys.writes();
    assert_eq!(after_partial.len(), 2);
    let revoked = write_bytes(&mut session.owner, &session.settings, session.pty, b"after");
    assert!(revoked.error.is_some(), "ownership was revoked");
    assert_eq!(
        ptys.writes(),
        after_partial,
        "and the revoked connection reached the host no further"
    );

    // ---- arm 6b: a failed host write revokes ownership too ------------------------------------
    //
    // **`Partial` is not a stand-in for `Failed` and arm 6a cannot cover this.** The write path
    // revokes on `Partial` *or* `Failed`, and the two arrive by different routes: a short count is
    // `Ok(n)` from the host, an outright refusal is `Err`. Measured, and this is why the arm
    // exists: mapping `Err(_)` to `Complete { written_len: bytes.len() }` left the **entire
    // workspace green** while the daemon recorded success for bytes the host refused, kept the
    // connection's ownership, and answered the caller `Ok`.
    //
    // It runs on a second attachment because arm 6a revoked the first. That is not a workaround —
    // it is the same revocation asserted twice, once per outcome, each on a connection that really
    // held the handle.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while count(&fixture, |record| matches!(record, Record::Detached { .. })) == detached_before {
        assert!(
            std::time::Instant::now() < deadline,
            "the partially written attachment was torn down"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Arm 5's replanted goal issues a fresh turn the moment the first attachment is released, and
    // `Command::Attach` refuses an agent with an open turn. Quiesce it, so the refusal this arm
    // could otherwise take for a result is the one it means to test and not a scheduling artefact.
    let reattached = quiesce_and_attach(&fixture, &mut session);

    ptys.fail_next_write();
    let failed = write_bytes(
        &mut session.sibling,
        &session.settings,
        reattached.pty,
        REFUSED,
    );
    assert!(
        failed.error.is_some(),
        "a refused host write is not reported as success"
    );

    let host_after_failed = ptys.writes();
    assert_eq!(
        host_after_failed.len(),
        after_partial.len() + 1,
        "the failed call still reached the host exactly once"
    );
    assert_eq!(
        host_after_failed.last(),
        Some(&(reattached.pty, REFUSED.to_vec())),
        "and it carried the caller's exact bytes"
    );

    let failed_completions: Vec<dudes_types::Lsn> = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::PtyWriteCompleted {
                request_lsn,
                outcome: dudes_proto::PtyWriteOutcome::Failed { .. },
            } => Some(request_lsn),
            _ => None,
        })
        .collect();
    assert_eq!(
        failed_completions.len(),
        1,
        "the host's refusal is journaled as what it was, once"
    );
    // Resolved against this journal's own positions, not against a count or an ordering: the
    // completion has to name the request it completes.
    let named = requests(&fixture)
        .into_iter()
        .find(|(lsn, _)| *lsn == failed_completions[0])
        .map(|(_, record)| record)
        .expect("the failed completion names a request in this journal");
    assert!(
        matches!(
            &named,
            Record::PtyWriteRequested { pty, payload, .. }
                if *pty == reattached.pty
                    && payload.decoded_len == REFUSED.len() as u64
                    && payload.blake3 == blake3::hash(REFUSED).to_hex().to_string()
        ),
        "and the request it names is the refused write's own: {named:?}"
    );

    let after_failed = write_bytes(
        &mut session.sibling,
        &session.settings,
        reattached.pty,
        b"after-failure",
    );
    assert!(
        after_failed.error.is_some(),
        "a failed host write revokes this connection's ownership, as a partial one does"
    );
    assert_eq!(
        ptys.writes(),
        host_after_failed,
        "and the revoked connection reached the host no further"
    );

    let durable_before_restart = fixture.entries();
    let last_lsn = durable_before_restart
        .last()
        .map(|(lsn, _)| *lsn)
        .expect("a non-empty journal");
    drop(unproven);
    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );

    // ---- arm 6c: replay after a completed write repeats no bytes ------------------------------
    let replayed = SharedPtyHost::new(fixture.journal_dir());
    let replayed_lsn = replay_run(&fixture, replayed.clone());
    assert!(
        replayed_lsn >= last_lsn,
        "the restarted daemon replayed to the end of the journal ({replayed_lsn:?} < {last_lsn:?})"
    );
    assert_eq!(
        replayed.writes(),
        Vec::new(),
        "replay wrote no bytes for a request that already has an outcome"
    );
    assert_eq!(
        replayed.shutdowns(),
        1,
        "control: that run really held this host, so its empty write log is evidence"
    );

    // ---- arm 6d: replay after a request with no outcome repeats no bytes ----------------------
    //
    // The crash window: the request is durable, the host was called or was not,
    // and nothing says which. Replay must invent neither answer, and must not re-send the bytes.
    let orphan_lsn = seed_orphan_request(&fixture, session.pty);
    let orphaned = SharedPtyHost::new(fixture.journal_dir());
    let orphan_replay_lsn = replay_run(&fixture, orphaned.clone());
    // Without this the arm's green would also be satisfied by a run that stopped short of the
    // seeded request — "no bytes were repeated" is cheap to satisfy by never reading the record.
    assert!(
        orphan_replay_lsn >= orphan_lsn,
        "the restarted daemon read past the orphaned request ({orphan_replay_lsn:?} < {orphan_lsn:?})"
    );
    assert_eq!(
        orphaned.writes(),
        Vec::new(),
        "an unmatched request is an audit fact, not an instruction"
    );
    assert_eq!(
        orphaned.shutdowns(),
        1,
        "control: this host was the wired one"
    );
    assert!(
        !fixture.records().iter().any(|record| matches!(
            record,
            Record::PtyWriteCompleted { request_lsn, .. } if *request_lsn == orphan_lsn
        )),
        "replay invented no outcome for the unmatched request"
    );
}

/// Pause the agent, end whatever turn is open, and attach `sibling` once the daemon admits it.
///
/// `Command::Attach` refuses an agent whose `open_turn` is set, and a goal set while a connection
/// held the keyboard issues a fresh turn the moment that attachment is released. Pausing is what
/// makes the drain terminate: with no new turn issuable, the one in flight is the last there will
/// be.
///
/// The final poll is a barrier and not a second subject. A refused `agent.attach` journals nothing
/// — `agent_attach_binds_the_durable_attachment_to_one_connection` asserts exactly that for the
/// no-session and bad-dimension cells — so retrying it cannot manufacture the state it is waiting
/// for. It is here because the drain reads the journal while the decide loop writes it, and a turn
/// already decided but not yet appended is invisible to any journal-shaped check.
fn quiesce_and_attach(fixture: &Fixture, session: &mut Attached) -> AttachResult {
    let paused = session
        .owner
        .call_as_operator(
            Method::LoopPause,
            Some(serde_json::json!({ "agent": session.agent })),
            credential_of(&session.settings),
        )
        .expect("a response");
    assert!(paused.error.is_none(), "loop.pause: {paused:?}");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let records = fixture.records();
        let ended: std::collections::BTreeSet<_> = records
            .iter()
            .filter_map(|record| match record {
                Record::TurnEnded { turn, .. } => Some(*turn),
                _ => None,
            })
            .collect();
        if let Some((turn, resumed)) = records.iter().rev().find_map(|record| match record {
            Record::TurnIssued { turn, session, .. } if !ended.contains(turn) => {
                Some((*turn, session.clone()))
            }
            _ => None,
        }) {
            // Confirm the resume before ending the turn. A turn that resumes a session and exits
            // without the vendor confirming it leaves `unconfirmed_resume` set, and
            // the daemon then refuses to attach, which is a refusal for a state this test
            // does not mean to create.
            if let Some(resumed) = resumed {
                session.running.handle.input(Input::SessionLearned {
                    turn,
                    session: resumed,
                });
                fixture.await_record("the resumed session was confirmed", |record| match record {
                    Record::SessionLearned { turn: seen, .. } if *seen == turn => Some(()),
                    _ => None,
                });
            }
            common::end_turn_with_native(fixture, &session.running.handle, &session.host, turn);
            fixture.await_record("the open turn ended", |record| match record {
                Record::TurnEnded { turn: seen, .. } if *seen == turn => Some(()),
                _ => None,
            });
        }

        let response = attach(&mut session.sibling, &session.settings, session.agent);
        if response.error.is_none() {
            return serde_json::from_value(response.result.expect("attach result")).unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a paused agent with a confirmed session and no open turn is attachable: {response:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Append one mail with an inline `body`, before any daemon opens this journal.
fn seed_mail(fixture: &Fixture, body: &str) {
    let options = dudes_journal::JournalOptions {
        fsync: dudes_journal::Fsync::Full,
        checkpoint_every: 0,
        ..dudes_journal::JournalOptions::default()
    };
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), options).expect("open the seed journal");
    writer
        .append(&dudes_proto::Entry {
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_600_000),
            record: Record::Mail {
                thread: None,
                thread_seq: 0,
                members: Vec::new(),
                id: dudes_types::MailId::parse("01ARZ3NDEKTSV4RRFFQ69G5FB9").expect("a mail id"),
                from: dudes_proto::MailFrom::Human,
                to: dudes_proto::MailTo::Human,
                act: dudes_proto::MailAct::Message,
                subject: "planted".to_owned(),
                body: Some(body.to_owned()),
                body_len: body.len() as u64,
                body_path: None,
                body_blake3: None,
                state: dudes_types::MailState::Durable,
                actor: None,
                provenance: None,
            },
        })
        .expect("seed the mail");
}

/// Append one `PtyWriteRequested` with no completion, and return the position it was given.
fn seed_orphan_request(fixture: &Fixture, pty: dudes_proto::rpc::pty::PtyId) -> dudes_types::Lsn {
    let options = dudes_journal::JournalOptions {
        fsync: dudes_journal::Fsync::Full,
        checkpoint_every: 0,
        ..dudes_journal::JournalOptions::default()
    };
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), options).expect("open the seed journal");
    writer
        .append(&dudes_proto::Entry {
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_700_000),
            record: Record::PtyWriteRequested {
                pty,
                actor: Actor::Peer {
                    pid: 4242,
                    uid: None,
                },
                provenance: Some(PeerProvenance::NoRecordedAgent),
                payload: dudes_proto::PayloadDigest {
                    decoded_len: 5,
                    blake3: blake3::hash(b"crash").to_hex().to_string(),
                },
            },
        })
        .expect("seed the orphan request")
}

fn seed_orphan_close_request(fixture: &Fixture, pty: dudes_proto::rpc::pty::PtyId) {
    let options = dudes_journal::JournalOptions {
        fsync: dudes_journal::Fsync::Full,
        checkpoint_every: 0,
        ..dudes_journal::JournalOptions::default()
    };
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), options).expect("open the seed journal");
    writer
        .append(&dudes_proto::Entry {
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_700_000),
            record: Record::PtyCloseRequested {
                pty,
                actor: Actor::System,
                provenance: None,
                reason: "orphan replay control".to_owned(),
            },
        })
        .expect("seed one unmatched close request");
}

fn seed_replayed_attached(fixture: &Fixture, agent: AgentId) -> dudes_types::Lsn {
    let options = dudes_journal::JournalOptions {
        fsync: dudes_journal::Fsync::Full,
        checkpoint_every: 0,
        ..dudes_journal::JournalOptions::default()
    };
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), options).expect("open the replay journal");
    writer
        .append(&dudes_proto::Entry {
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_800_000),
            record: Record::Attached {
                agent,
                by: Actor::System,
                hook_port: None,
                provenance: None,
            },
        })
        .expect("seed the crash-prefix Attached")
}

#[test]
#[cfg(debug_assertions)]
fn gone_attachment_short_write_keeps_operator_diagnostics_available() {
    use dudes_supervisor::host::ProbeAnswer;
    use dudes_types::{Lsn, Timestamp};

    let fixture = Fixture::new();
    let settings = fixture.settings();
    let first = duded::boot(
        &settings,
        common::seams(
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let mut first = common::start(first);
    let agent = common::hire(&first.handle, fixture.state.path());
    first.stop(duded::ExitCause::Ended);
    seed_replayed_attached(&fixture, agent);
    let (mut writer, _) = dudes_journal::open(
        &fixture.journal_dir(),
        dudes_journal::JournalOptions::default(),
    )
    .unwrap();
    writer
        .append(&dudes_proto::Entry {
            at: Timestamp::from_unix_millis(1_787_394_800_001),
            record: Record::PtySpawned {
                agent,
                pty: dudes_proto::rpc::pty::PtyId(999),
                pid: 70_001,
                start_time: 900,
                boot: None,
            },
        })
        .unwrap();
    drop(writer);

    let host = SharedHost::new();
    host.answer(70_001, ProbeAnswer::StartValue(900));
    let daemon = duded::boot(
        &settings,
        common::seams(
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let failure = daemon.append_failure();
    let mut running = common::start(daemon);
    let before = fixture.entries();
    running.handle.input(Input::Tick);
    assert!(running.handle.doctor().unwrap().journal_stopped.is_none());
    assert_eq!(
        fixture.entries(),
        before,
        "an idle healthy Tick writes nothing"
    );

    let mut files: Vec<_> = std::fs::read_dir(fixture.journal_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    files.sort();
    let file = files.last().unwrap();
    let segment = file
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .parse::<u32>()
        .unwrap();
    let position = std::fs::metadata(file).unwrap().len();
    let attempted = Lsn::from_parts(segment, position).unwrap();
    failure.short_write_next(5);
    host.answer(70_001, ProbeAnswer::NoSuchPid);
    running.handle.input(Input::Tick);
    let doctor = running
        .handle
        .doctor()
        .expect("the follow-up detach failure leaves diagnostics available");
    assert_eq!(doctor.journal_stopped.unwrap().lsn, attempted);
    assert_eq!(std::fs::metadata(file).unwrap().len(), position + 5);
    running.handle.input(Input::Tick);
    assert_eq!(
        running
            .handle
            .doctor()
            .unwrap()
            .journal_stopped
            .unwrap()
            .lsn,
        attempted
    );
    assert_eq!(
        std::fs::metadata(file).unwrap().len(),
        position + 5,
        "no retry after poison"
    );
    running.stop(duded::ExitCause::Ended);
    assert_eq!(host.shutdowns(), 1);
}

/// Boot a second daemon over the same state directory, let it settle, and stop it.
///
/// Returns the lsn the restarted daemon reports, which is how a caller tells a run that replayed
/// the journal from one that failed to open it.
fn replay_run(fixture: &Fixture, ptys: SharedPtyHost) -> dudes_types::Lsn {
    let settings = fixture.settings();
    let column = ScriptedColumn::new(PeerProvenance::NoRecordedAgent);
    let seams = duded::Seams {
        host: SharedHost::new(),
        pty_host: Box::new(ptys),
        predicates: Arc::new(ScriptedRunner::scripted([common::passes()])),
        clock: Arc::new(TestClock::new()),
        rng: FakeRng::new(),
        resolver: duded::classify::Resolver::fixed(
            Arc::clone(&column) as Arc<dyn dudes_rpc::PeerResolver>
        ),
        adapters: dudes_providers::registry::Adapters::from_config(
            &dudes_providers::registry::ProvidersConfig {
                claude: Some(dudes_providers::claude::config::ClaudeConfig::default()),
            },
            &[],
        )
        .expect("default Claude adapter"),
        children: duded::ChildEnds::new(),
        witness: Witness::default(),
    };
    let daemon = duded::boot(&settings, seams).expect("the same state directory boots again");
    let mut running = common::start(daemon);
    let mut client = Client::connect(settings.socket()).expect("connect to the restarted daemon");
    let ping = client
        .call(Method::SystemPing, None)
        .expect("the restarted daemon answers");
    assert!(ping.error.is_none(), "system.ping: {ping:?}");
    let state: dudes_proto::rpc::system::PingResult =
        serde_json::from_value(ping.result.expect("a ping result")).unwrap();
    drop(client);
    assert_eq!(
        running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
    state.lsn
}

/// Invariant 7 on the attached launch: the PTY host is handed the program the daemon found on the
/// launch's `PATH`, never the bare name a child would search for from inside the workspace.
#[test]
fn an_attach_hands_the_pty_host_the_program_found_on_path() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut session = attach_one(&fixture, &ptys);
    let opened: Vec<_> = ptys
        .opens()
        .into_iter()
        .map(|(spec, _, _)| spec.argv[0].clone())
        .collect();
    let stand_in = fixture.runtime.path().join("bin").join(if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    });
    assert_eq!(opened, [stand_in.into_os_string()]);
    assert_eq!(
        session.running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
}

#[cfg(test)]
mod native_close_query_tests {
    use super::{Fixture, SharedPtyHost};
    use duded::ports::{PtyCloseOutcome, PtyHostPort, PtyNativeBoundaryUnknown};
    use dudes_providers::launch::LaunchSpec;
    use std::sync::mpsc;
    use std::time::Duration;

    fn open_mock(fixture: &Fixture) -> (SharedPtyHost, dudes_proto::rpc::pty::PtyId) {
        let mut host = SharedPtyHost::with_native_evidence(fixture.journal_dir());
        let opened = host
            .open(
                LaunchSpec {
                    argv: vec!["mock-pty".into()],
                    env: Vec::new(),
                    cwd: fixture.state.path().to_path_buf(),
                    stdin: dudes_providers::launch::LaunchInput::Null,
                },
                80,
                24,
            )
            .unwrap();
        (host, opened.pty)
    }

    #[test]
    fn a_held_close_barrier_makes_native_query_unknown_without_unblocking_close() {
        let fixture = Fixture::new();
        let (mut host, pty) = open_mock(&fixture);
        let token = host.track_native_close(pty).unwrap();
        let held = host.block_host_calls();
        let (queried, query) = mpsc::channel();
        let mut query_host = host.clone();
        let query_token = token.clone();
        let query_thread = std::thread::spawn(move || {
            queried
                .send(query_host.native_close_absence(pty, &query_token))
                .unwrap();
        });
        let (started, close_started) = mpsc::channel();
        let (closed, close) = mpsc::channel();
        let mut close_host = host.clone();
        let close_thread = std::thread::spawn(move || {
            started.send(()).unwrap();
            closed.send(close_host.close(pty)).unwrap();
        });
        let close_entered = close_started.recv_timeout(Duration::from_secs(1));
        let query_before_release = query.recv_timeout(Duration::from_secs(1));
        let close_before_release = close.try_recv();
        let records_before_release = fixture.records();
        drop(held);
        query_thread.join().unwrap();
        close_thread.join().unwrap();
        assert!(close_entered.is_ok());
        assert_eq!(
            query_before_release,
            Ok(Err(PtyNativeBoundaryUnknown::Unconfirmed))
        );
        assert!(matches!(
            close_before_release,
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(records_before_release.is_empty());
        assert!(matches!(close.recv().unwrap(), PtyCloseOutcome::Closed));
        assert_eq!(host.closes(), vec![pty]);
        assert!(host.native_close_absence(pty, &token).is_ok());
        assert!(!host.acknowledge_native_close(pty, &token));
        assert_eq!(host.track_native_close(pty).unwrap(), token);
        assert!(!host.native_acknowledged(pty));
        assert!(fixture.records().is_empty());
    }

    #[test]
    fn a_poisoned_mock_lock_does_not_supply_native_absence() {
        let fixture = Fixture::new();
        let (mut host, pty) = open_mock(&fixture);
        let token = host.track_native_close(pty).unwrap();
        let poisoned_host = host.clone();
        assert!(
            std::thread::spawn(move || {
                let _held = poisoned_host.block_host_calls();
                panic!("poison the mock state before any native reap");
            })
            .join()
            .is_err()
        );
        assert_eq!(
            host.native_close_absence(pty, &token),
            Err(PtyNativeBoundaryUnknown::Unconfirmed)
        );
        assert!(!host.acknowledge_native_close(pty, &token));
        assert_eq!(host.track_native_close(pty).unwrap(), token);
        assert!(matches!(host.close(pty), PtyCloseOutcome::Closed));
        assert!(host.native_close_absence(pty, &token).is_ok());
        assert!(!host.acknowledge_native_close(pty, &token));
    }
}
