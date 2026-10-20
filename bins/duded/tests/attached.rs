//! The connection-bound `agent.attach` path through the production dispatcher and daemon loop.

#![cfg(unix)]

mod common;

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
        rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
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

#[test]
fn direct_daemon_attachments_after_two_reboots_reserve_token_only_history() {
    let fixture = Fixture::new();
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let Hired {
        settings,
        mut running,
        agent,
        owner,
        sibling,
        ..
    } = hired(&fixture, &ptys, fixture.settings());
    running.handle.ask(duded::ports::Ask::Pause(agent)).unwrap();
    drop((owner, sibling));
    assert_eq!(
        running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );

    let (second_hash, second_token) = reboot_and_attach_directly(&fixture, &settings, agent);
    let (third_hash, third_token) = reboot_and_attach_directly(&fixture, &settings, agent);
    assert_ne!(second_hash, third_hash);
    assert_ne!(second_token.expose(), third_token.expose());
}

fn journaled_ulids(
    entries: &[(dudes_types::Lsn, dudes_proto::Entry)],
) -> std::collections::BTreeSet<ulid::Ulid> {
    use serde_json::Value;

    let mut pending = vec![serde_json::to_value(entries).unwrap()];
    let mut ids = std::collections::BTreeSet::new();
    while let Some(value) = pending.pop() {
        match value {
            Value::String(text) => {
                if let Ok(id) = ulid::Ulid::from_string(&text)
                    && id.to_string() == text
                {
                    ids.insert(id);
                }
            }
            Value::Array(values) => pending.extend(values),
            Value::Object(values) => pending.extend(
                values
                    .into_iter()
                    .flat_map(|(key, value)| [Value::String(key), value]),
            ),
            _ => {}
        }
    }
    ids
}

#[expect(
    clippy::too_many_lines,
    reason = "one boot keeps its prefix, mint observer, native PTY, and socket barriers together"
)]
fn reboot_and_attach_directly(
    fixture: &Fixture,
    settings: &duded::Settings,
    agent: AgentId,
) -> (String, dudes_proto::rpc::hook::HookToken) {
    use duded::ports::{Answered, Ask};

    let prefix = fixture.entries();
    let ids_before = journaled_ulids(&prefix);
    assert!(ids_before.iter().all(|id| id.timestamp_ms() == 0));
    let highest_id = u128::from(*ids_before.last().unwrap());
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let ptys = SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut parts = common::seams(
        settings,
        host,
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    parts.pty_host = Box::new(ptys.clone());
    parts.resolver = common::native_resolver(&ptys);
    let tokens = parts.rng.tokens();
    let daemon = duded::boot(settings, parts).expect("replay the confirmed, paused session");
    assert!(
        daemon.operator_authority().is_sealed(),
        "recorded agents seal new socket effect authority; this test uses direct daemon input"
    );
    let mut running = common::start(daemon);
    let mut client = Client::connect(settings.socket()).expect("a real rebooted socket");
    assert!(
        client
            .call(Method::SystemPing, None)
            .unwrap()
            .error
            .is_none()
    );
    let Answered::Attached(pty) = running
        .handle
        .ask(Ask::Attach {
            agent,
            by: Actor::System,
            provenance: PeerProvenance::NoRecordedAgent,
            cols: 101,
            rows: 37,
        })
        .expect("the existing confirmed session attaches")
    else {
        panic!("the direct daemon command did not attach")
    };
    let attached = fixture.entries();
    assert_eq!(journaled_ulids(&attached), ids_before);
    let tail = &attached[prefix.len()..];
    let mints: Vec<_> = tail
        .iter()
        .enumerate()
        .filter_map(|(index, (_, entry))| match &entry.record {
            Record::HookTokenMinted {
                agent: seen,
                token_hash,
            } if *seen == agent => Some((index, token_hash.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(mints.len(), 1);
    let (mint_at, hash) = &mints[0];
    assert!(
        tail[..*mint_at].iter().all(|(_, entry)| matches!(
            &entry.record,
            Record::Attached { agent: seen, .. }
                | Record::LoopDecided {
                    agent: seen,
                    decision: dudes_proto::LoopDecision::Hold { .. },
                    ..
                } if *seen == agent
        )),
        "no new identity-bearing record precedes the token mint: {tail:?}"
    );
    let token = tokens.token_of(hash);
    let token_counter: u128 = token.expose().trim_start_matches('A').parse().unwrap();
    assert!(token_counter > highest_id);
    let opens = ptys.opens();
    assert_eq!(opens.len(), 1);
    assert!(opens[0].0.env.iter().any(|variable| {
        variable.name() == "DUDES_TOKEN" && variable.expose_value() == token.expose()
    }));
    assert!(matches!(
        running
            .handle
            .ask(Ask::Detach {
                agent,
                pty,
                by: Actor::System,
                provenance: PeerProvenance::NoRecordedAgent,
            })
            .unwrap(),
        Answered::PtyClosed(dudes_proto::PtyCloseOutcome::Closed)
    ));
    let detached = fixture.entries();
    assert_eq!(journaled_ulids(&detached), ids_before);
    assert!(detached[prefix.len()..].iter().any(|(_, entry)| {
        matches!(entry.record, Record::Detached { agent: seen, .. } if seen == agent)
    }));
    assert_eq!(ptys.closes(), vec![pty]);
    assert!(ptys.native_acknowledged(pty));
    let response = client
        .call_as_operator(Method::AgentList, None, credential_of(settings))
        .expect("the rebooted socket answers an authenticated status request");
    assert!(response.error.is_none(), "{response:?}");
    let agents: Vec<dudes_proto::projection::AgentView> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        agents.iter().map(|agent| agent.id).collect::<Vec<_>>(),
        vec![agent]
    );
    drop(client);
    assert_eq!(
        running.stop(duded::ExitCause::Ended),
        duded::ExitCause::Ended
    );
    assert_eq!(journaled_ulids(&fixture.entries()), ids_before);
    (hash.clone(), token)
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
    let fixture_parts = common::seams(
        &settings,
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    let first = duded::boot(&settings, fixture_parts).unwrap();
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
            &settings,
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
