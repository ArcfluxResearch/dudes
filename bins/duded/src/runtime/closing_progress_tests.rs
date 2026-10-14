use super::work_test_support as common;
use super::*;
use crate::floor_stream::QueueRead;
use crate::settings::FloorSettings;

fn settings(fixture: &common::Fixture) -> crate::Settings {
    let mut settings = fixture.settings();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/floor-release-v3"
        )),
        port: Some(port),
    });
    settings
}

type TestDaemon = crate::Daemon<
    common::SharedHost,
    common::ScriptedRunner,
    common::TestClock,
    dudes_supervisor::testing::FakeRng,
>;

fn boot_daemon(fixture: &common::Fixture) -> TestDaemon {
    crate::boot(
        &settings(fixture),
        common::seams(
            common::SharedHost::new(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap()
}

fn hire_open(daemon: &mut TestDaemon, fixture: &common::Fixture) -> HireResult {
    let Answered::Hired(hired) = daemon
        .ask(Ask::Hire(Box::new(common::hire_params(
            fixture.state.path(),
        ))))
        .unwrap()
    else {
        panic!("hire");
    };
    daemon
        .ask(Ask::SetGoal(Box::new(common::goal_params(
            hired.agent,
            &["/usr/bin/true"],
        ))))
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !daemon.sup.world().agents[&hired.agent]
        .open_turn
        .is_some_and(|turn| daemon.sup.world().turns[&turn].is_open_child())
    {
        pump_fixture_job(daemon, deadline);
    }
    hired
}

fn pump_fixture_job(daemon: &mut TestDaemon, deadline: std::time::Instant) {
    match daemon
        .jobs
        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        .expect("the actual fixture worker must finish within its deadline")
    {
        Job::Input(input) => daemon.pump(*input).unwrap(),
        Job::PredicateEnded { goal, verdict } => daemon.predicate_ended(goal, verdict).unwrap(),
        _ => panic!("unexpected fixture job"),
    }
}

fn observed_end(turn: TurnId) -> Input {
    Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: dudes_proto::ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: dudes_proto::TurnOutcome::Completed,
        last_message: None,
    }
}

#[test]
fn a_deferred_cut_failure_keeps_its_initiating_error_and_abandons_output() {
    let fixture = common::Fixture::new();
    let settings = settings(&fixture);
    let mut daemon = crate::boot(
        &settings,
        common::seams(
            common::SharedHost::new(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let hired = hire_open(&mut daemon, &fixture);
    daemon
        .closing_request(crate::closing::Request::Announce)
        .unwrap();
    let streams = daemon.ctx.floor_streams.as_ref().unwrap().clone();
    let opened = streams.subscribe().unwrap();
    let turn = daemon.sup.world().agents[&hired.agent].open_turn.unwrap();
    let folded = daemon.sup.world().last_applied_lsn;
    let output = daemon.ctx.children.output();
    let before = output.snapshot(hired.agent);
    daemon
        .onboarding_failure()
        .arm(OnboardingFailurePoint::SpendRefresh);
    let failure = daemon
        .pump(Input::ChildExited {
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: dudes_proto::ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: None,
            model_usage: BTreeMap::new(),
            outcome: dudes_proto::TurnOutcome::Completed,
            last_message: None,
        })
        .unwrap_err();
    assert_eq!(
        failure.to_string(),
        "injected durable spend refresh failure"
    );
    assert_eq!(daemon.sup.world().last_applied_lsn, folded);
    assert!(daemon.ctx.pending_floor.is_empty());
    assert!(daemon.ctx.pending_floor_base.is_none());
    assert!(matches!(
        opened.subscription.next(Duration::ZERO),
        QueueRead::Ended
    ));
    assert!(streams.subscribe().is_err());
    assert_eq!(output.snapshot(hired.agent).revision, before.revision);
    assert_eq!(output.snapshot(hired.agent).state, before.state);
    let mut running = common::start(daemon);
    running.stop(ExitCause::ShutdownRpc);
}

#[test]
fn failure_serializing_the_last_folded_patch_keeps_its_original_context() {
    let fixture = common::Fixture::new();
    let mut daemon = boot_daemon(&fixture);
    let hired = hire_open(&mut daemon, &fixture);
    daemon
        .closing_request(crate::closing::Request::Announce)
        .unwrap();
    let streams = daemon.ctx.floor_streams.as_ref().unwrap().clone();
    let opened = streams.subscribe().unwrap();
    let entry = Entry {
        at: daemon.ctx.clock.now(),
        record: Record::AgentRenamed {
            id: hired.agent,
            name: "x".repeat(dudes_proto::rpc::MAX_FRAME_BYTES - 256),
            browser_rename: None,
        },
    };
    let lsn = daemon.ctx.append(daemon.sup.world(), &entry).unwrap();
    assert_eq!(daemon.ctx.pending_floor.len(), 1);
    let failure = daemon
        .pump_inputs(
            &mut PumpReport::Discard,
            PumpOrigin::Ordinary,
            VecDeque::from([Input::Replayed { lsn, entry }]),
            |_| Ok(()),
        )
        .unwrap_err();
    assert_eq!(
        failure.to_string(),
        "publishing the folded closing Floor projection"
    );
    assert_eq!(
        failure.root_cause().to_string(),
        "Floor browser event exceeds 1 MiB"
    );
    assert_eq!(daemon.sup.world().last_applied_lsn, Some(lsn));
    assert!(daemon.ctx.pending_floor.is_empty());
    assert!(daemon.ctx.pending_floor_base.is_none());
    assert!(matches!(
        opened.subscription.next(Duration::ZERO),
        QueueRead::Ended
    ));
    assert!(streams.subscribe().is_err());
    let mut running = common::start(daemon);
    running.stop(ExitCause::ShutdownRpc);
}

#[test]
fn a_board_record_during_closing_drains_its_cut_before_immediate_retirement() {
    let fixture = common::Fixture::new();
    let mut daemon = boot_daemon(&fixture);
    let mut agents = Vec::new();
    for _ in 0..2 {
        let Answered::Hired(hired) = daemon
            .ask(Ask::Hire(Box::new(common::hire_params(
                fixture.state.path(),
            ))))
            .unwrap()
        else {
            panic!("hire");
        };
        daemon
            .ask(Ask::SetGoal(Box::new(common::goal_params(
                hired.agent,
                &["/usr/bin/true"],
            ))))
            .unwrap();
        agents.push(hired.agent);
    }
    daemon
        .closing_request(crate::closing::Request::Announce)
        .unwrap();
    let observed = daemon.ctx.floor_closing.unwrap().progress;
    let cut = daemon.sup.world().last_applied_lsn;
    daemon
        .pump(Input::Work {
            agent: agents[0],
            view: dudes_policy::WorkInput::default(),
        })
        .unwrap();
    assert_eq!(daemon.sup.world().last_applied_lsn, cut);
    assert_eq!(
        daemon.ctx.floor_closing.unwrap().progress,
        observed,
        "non-folding inputs do not refresh progress"
    );
    daemon
        .ask(Ask::BoardCreate {
            request: CardCreateParams {
                title: "Owned board change".into(),
                department: "engineering".into(),
                deps: vec![],
            },
            by: Actor::Peer {
                pid: 4242,
                uid: Some(0),
            },
        })
        .unwrap();
    let retired = daemon.pump(Input::Command(Command::Retire {
        agent: agents[0],
        reason: "Owned immediate retirement".into(),
        browser_retire: None,
    }));
    assert!(
        retired.is_ok(),
        "inner retirement publication failure: {:#}",
        retired.unwrap_err()
    );
    assert!(daemon.ctx.pending_floor.is_empty());
    assert!(daemon.ctx.pending_floor_base.is_none());
    assert!(
        daemon
            .ctx
            .floor_streams
            .as_ref()
            .unwrap()
            .subscribe()
            .is_ok()
    );
    let mut running = common::start(daemon);
    running.stop(ExitCause::ShutdownRpc);
}

#[test]
fn complete_stream_snapshot_keeps_final_counts_and_positive_time_after_waiting_is_cleared() {
    let fixture = common::Fixture::new();
    let mut daemon = boot_daemon(&fixture);
    let hired = hire_open(&mut daemon, &fixture);
    let crate::closing::Answer::Announced(started) = daemon
        .closing_request(crate::closing::Request::Announce)
        .unwrap()
    else {
        panic!("announcement");
    };
    daemon
        .closing_request(crate::closing::Request::Ack {
            announcement: started.announcement,
            agent: hired.agent,
        })
        .unwrap();
    let view = daemon.ctx.floor_closing.unwrap();
    assert_eq!(
        (
            view.progress.acknowledged,
            view.progress.owed,
            view.progress.running
        ),
        (1, 0, 1)
    );
    assert!(
        !daemon
            .sup
            .world()
            .closing
            .as_ref()
            .unwrap()
            .ready(daemon.sup.world())
    );
    daemon.closing.responses = 0;
    assert!(!daemon.closing_tick().unwrap());
    let turn = daemon.sup.world().agents[&hired.agent].open_turn.unwrap();
    daemon.pump(observed_end(turn)).unwrap();
    assert!(daemon.closing_tick().unwrap());
    let final_progress = daemon.ctx.floor_closing.unwrap().progress;
    assert_eq!(
        (
            final_progress.acknowledged,
            final_progress.settled,
            final_progress.owed,
            final_progress.running
        ),
        (1, 0, 0, 0)
    );
    assert!(final_progress.remaining_ms > 0);
    let waiting = (
        daemon.closing.deadline.take(),
        daemon.closing.announcement.take(),
    );
    let opened = daemon
        .ctx
        .floor_streams
        .as_ref()
        .unwrap()
        .subscribe_with_siblings(
            crate::floor_stream::Siblings::default(),
            None,
            Timestamp::from_unix_millis(i64::MAX),
        )
        .unwrap();
    let frame: serde_json::Value =
        serde_json::from_slice(crate::floor_websocket::text_payload(&opened.snapshot).unwrap())
            .unwrap();
    assert_eq!(
        frame["projection"]["closing_time"],
        serde_json::json!({ "kind": "complete" })
    );
    assert_eq!(
        frame["projection"]["closing_progress"],
        serde_json::to_value(final_progress).unwrap()
    );
    daemon.closing.deadline = waiting.0;
    daemon.closing.announcement = waiting.1;
    let mut running = common::start(daemon);
    running.stop(ExitCause::ShutdownRpc);
}

#[test]
fn saving_memory_publishes_its_projection_cut_without_waiting_for_a_policy_fold() {
    let fixture = common::Fixture::new();
    let mut daemon = boot_daemon(&fixture);
    let hired = hire_open(&mut daemon, &fixture);
    daemon
        .closing_request(crate::closing::Request::Announce)
        .unwrap();
    let policy_cut = daemon.sup.world().last_applied_lsn;
    let progress = daemon.ctx.floor_closing.unwrap().progress;
    let Answered::MemoryAppended(lsn) = daemon
        .append_memory(
            hired.agent,
            b"Owned saving note",
            dudes_proto::PeerProvenance::Agent(hired.agent),
        )
        .unwrap()
    else {
        panic!("memory append");
    };
    assert_eq!(daemon.sup.world().last_applied_lsn, policy_cut);
    assert!(
        daemon.ctx.pending_floor.is_empty(),
        "memory has no later policy-fold callback"
    );
    assert!(daemon.ctx.pending_floor_base.is_none());
    assert_eq!(daemon.ctx.floor_closing.unwrap().progress, progress);
    let opened = daemon
        .ctx
        .floor_streams
        .as_ref()
        .unwrap()
        .subscribe()
        .unwrap();
    let frame: serde_json::Value =
        serde_json::from_slice(crate::floor_websocket::text_payload(&opened.snapshot).unwrap())
            .unwrap();
    assert_eq!(frame["projection"]["lsn"], serde_json::json!(lsn));
    assert!(frame["projection"].get("closing_progress").is_some());
    let mut running = common::start(daemon);
    running.stop(ExitCause::ShutdownRpc);
}

#[test]
fn saving_memory_behind_a_policy_cut_drains_in_durable_order() {
    let fixture = common::Fixture::new();
    let mut daemon = boot_daemon(&fixture);
    let hired = hire_open(&mut daemon, &fixture);
    daemon
        .closing_request(crate::closing::Request::Announce)
        .unwrap();
    let opened = daemon
        .ctx
        .floor_streams
        .as_ref()
        .unwrap()
        .subscribe()
        .unwrap();
    let _ = daemon.sup.handle(Input::Command(Command::Pause {
        agent: hired.agent,
        browser: None,
    }));
    let mut effects = daemon.sup.take_effects();
    assert_eq!(effects.len(), 1);
    let Effect::Persist { token, entry } = effects.pop().unwrap() else {
        panic!("pause append");
    };
    let policy_lsn = daemon.ctx.append(daemon.sup.world(), &entry).unwrap();
    let prepared = daemon
        .ctx
        .memory
        .prepare_append(
            hired.agent,
            b"Queued saving note",
            dudes_proto::PeerProvenance::Agent(hired.agent),
        )
        .unwrap();
    let memory_lsn = daemon
        .append_memory_entry(prepared.record().clone(), "append")
        .unwrap();
    assert_eq!(daemon.ctx.pending_floor.len(), 2);
    assert!(matches!(
        opened.subscription.next(Duration::ZERO),
        QueueRead::Empty
    ));
    daemon
        .pump(Input::Persisted {
            token,
            lsn: policy_lsn,
        })
        .unwrap();
    assert!(
        daemon.ctx.pending_floor.is_empty(),
        "the projection-only successor drains after the earlier policy cut"
    );
    assert!(daemon.ctx.pending_floor_base.is_none());
    assert_eq!(daemon.sup.world().last_applied_lsn, Some(policy_lsn));
    for expected in [policy_lsn, memory_lsn] {
        let QueueRead::Frame(bytes) = opened.subscription.next(Duration::ZERO) else {
            panic!("ordered durable cut");
        };
        let frame: serde_json::Value =
            serde_json::from_slice(crate::floor_websocket::text_payload(&bytes).unwrap()).unwrap();
        assert_eq!(frame["delta"]["lsn"], serde_json::json!(expected));
    }
    let snapshot = daemon
        .ctx
        .floor_streams
        .as_ref()
        .unwrap()
        .subscribe()
        .unwrap();
    let frame: serde_json::Value =
        serde_json::from_slice(crate::floor_websocket::text_payload(&snapshot.snapshot).unwrap())
            .unwrap();
    assert_eq!(frame["projection"]["lsn"], serde_json::json!(memory_lsn));
    let mut running = common::start(daemon);
    running.stop(ExitCause::ShutdownRpc);
}

#[test]
fn attached_writes_during_closing_publish_both_audit_cuts_without_replaying_bytes() {
    let fixture = common::Fixture::new();
    let host = common::SharedHost::with_native_evidence(fixture.journal_dir());
    let ptys = common::SharedPtyHost::with_native_evidence(fixture.journal_dir());
    let mut seams = common::seams(
        host.clone(),
        Arc::new(common::ScriptedRunner::default()),
        Arc::new(common::TestClock::new()),
        Witness::default(),
    );
    seams.pty_host = Box::new(ptys.clone());
    seams.resolver = common::native_resolver(&ptys);
    host.bind_child_ends(Arc::clone(&seams.children));
    let mut daemon = crate::boot(&settings(&fixture), seams).unwrap();
    let hired = hire_open(&mut daemon, &fixture);
    let turn = daemon.sup.world().agents[&hired.agent].open_turn.unwrap();
    let session = daemon.sup.world().turns[&turn].session.clone().unwrap();
    daemon
        .pump(Input::SessionLearned { turn, session })
        .unwrap();
    daemon
        .pump(Input::Command(Command::Pause {
            agent: hired.agent,
            browser: None,
        }))
        .unwrap();
    let key = host.peek(|host| host.key_for(daemon.sup.world().turns[&turn].pid.unwrap()));
    host.mark_native_reaped(&key);
    host.finish_owned_child(&key, observed_end(turn));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !host.native_acknowledged(&key)
        || !fixture.records().iter().any(|record| {
            matches!(record,
            Record::InstructionObservationFinished {
                owner: dudes_proto::InstructionObservationOwner::Managed { turn: ended, .. }, ..
            } if *ended == turn)
        })
    {
        pump_fixture_job(&mut daemon, deadline);
    }
    let actor = Actor::Peer {
        pid: 4242,
        uid: Some(0),
    };
    let provenance = Some(dudes_proto::PeerProvenance::NoRecordedAgent);
    let Answered::Attached(pty) = daemon
        .attach(hired.agent, actor, provenance, 80, 24)
        .unwrap()
    else {
        panic!("attach");
    };
    assert!(fixture.records().iter().any(|record| matches!(record,
        Record::PtySpawned { agent, pty: spawned, .. }
            if *agent == hired.agent && *spawned == pty)));
    daemon
        .closing_request(crate::closing::Request::Announce)
        .unwrap();
    let policy_cut = daemon.sup.world().last_applied_lsn;
    let progress = daemon.ctx.floor_closing.unwrap().progress;
    let bytes = b"owned input\n";
    let payload = dudes_proto::PayloadDigest {
        decoded_len: bytes.len() as u64,
        blake3: dudes_supervisor::blake3_hex(bytes),
    };
    let written = daemon
        .pty_write(pty, bytes, payload, actor, provenance)
        .unwrap();
    assert!(matches!(
        written,
        Answered::PtyWritten(dudes_proto::PtyWriteOutcome::Complete { .. })
    ));
    assert_eq!(ptys.writes(), [(pty, bytes.to_vec())]);
    assert_eq!(ptys.requested_before_write(), [true]);
    assert_eq!(daemon.sup.world().last_applied_lsn, policy_cut);
    assert!(
        daemon.ctx.pending_floor.is_empty(),
        "PTY audit records have no supervisor continuation"
    );
    assert!(daemon.ctx.pending_floor_base.is_none());
    assert_eq!(daemon.ctx.floor_closing.unwrap().progress, progress);
    let opened = daemon
        .ctx
        .floor_streams
        .as_ref()
        .unwrap()
        .subscribe()
        .unwrap();
    let frame: serde_json::Value =
        serde_json::from_slice(crate::floor_websocket::text_payload(&opened.snapshot).unwrap())
            .unwrap();
    assert_eq!(
        frame["projection"]["lsn"],
        serde_json::json!(daemon.ctx.projections.last_applied_lsn())
    );
    let mut running = common::start(daemon);
    running.stop(ExitCause::ShutdownRpc);
}

fn seed_closing(fixture: &common::Fixture, complete: bool) -> common::SharedHost {
    let mut source = dudes_supervisor::testing::Harness::new();
    let agents: Vec<_> = (0..3)
        .map(|_| source.hire_with_goal(&["/usr/bin/true"]).0)
        .collect();
    let (mut writer, _) = dudes_journal::open(
        &fixture.journal_dir(),
        dudes_journal::JournalOptions {
            fsync: dudes_journal::Fsync::Full,
            checkpoint_every: 0,
            ..dudes_journal::JournalOptions::default()
        },
    )
    .unwrap();
    let mut positions = BTreeMap::new();
    for mut entry in std::mem::take(&mut source.journal) {
        if let Record::AgentHired { cwd, .. } = &mut entry.record {
            *cwd = fixture.state.path().to_path_buf();
        }
        remap_seed_references(&mut entry.record, &positions);
        let lsn = writer.append(&entry).unwrap();
        positions.insert(Lsn(positions.len() as u64 + 1), lsn);
    }
    for phase in [
        dudes_proto::ClosingPhase::Announced,
        dudes_proto::ClosingPhase::Ack { agent: agents[0] },
    ] {
        writer
            .append(&Entry {
                at: Timestamp::from_unix_millis(1_787_394_600_000),
                record: Record::ClosingTime { phase },
            })
            .unwrap();
    }
    if complete {
        for agent in &agents {
            let goal = source.supervisor.world().agents[agent].goal.unwrap();
            let turn = source.open_turn(*agent).unwrap();
            source.drive(Input::Command(Command::GoalClear {
                agent: *agent,
                goal,
            }));
            source.drive(Input::ChildExited {
                final_reply: None,
                elapsed_ms: None,
                turn,
                exit: dudes_proto::ExitStatus {
                    code: Some(0),
                    signal: None,
                },
                usage: None,
                model_usage: BTreeMap::new(),
                outcome: dudes_proto::TurnOutcome::Completed,
                last_message: None,
            });
        }
        for mut entry in source.journal.clone() {
            remap_seed_references(&mut entry.record, &positions);
            let lsn = writer.append(&entry).unwrap();
            positions.insert(Lsn(positions.len() as u64 + 1), lsn);
        }
        writer
            .append(&Entry {
                at: Timestamp::from_unix_millis(1_787_394_600_100),
                record: Record::ClosingTime {
                    phase: dudes_proto::ClosingPhase::Complete,
                },
            })
            .unwrap();
    }
    drop(writer);
    common::SharedHost::with(source.host)
}

fn remap_seed_references(record: &mut Record, positions: &BTreeMap<Lsn, Lsn>) {
    let remap = |position: &mut Lsn| {
        *position = *positions.get(position).expect("an earlier fixture record");
    };
    match record {
        Record::TurnIssued {
            lsn_of_decision, ..
        } => remap(lsn_of_decision),
        Record::InstructionObservationFinished {
            baseline,
            settlement,
            ..
        } => {
            let dudes_proto::InstructionBaselineRef::Started { lsn } = baseline else {
                panic!("the fixture ends only its sampled managed attempts");
            };
            remap(lsn);
            let end = match settlement {
                dudes_proto::InstructionSettlement::Compared { end, .. } => Some(end),
                dudes_proto::InstructionSettlement::Unknown { end, .. } => end.as_mut(),
                dudes_proto::InstructionSettlement::NoProcess { proof_lsn, .. } => {
                    remap(proof_lsn);
                    None
                }
                dudes_proto::InstructionSettlement::NoIssuedTurn => None,
            };
            if let Some(end) = end {
                let dudes_proto::InstructionEndBoundary::ManagedTurn { end_lsn, .. } = end else {
                    panic!("the fixture ends only its managed children");
                };
                remap(end_lsn);
            }
        }
        _ => {}
    }
}

#[test]
fn restored_boot_snapshot_has_durable_counts_and_zero_monotonic_time_before_completion() {
    let fixture = common::Fixture::new();
    let host = seed_closing(&fixture, false);
    let daemon = crate::boot(
        &settings(&fixture),
        common::seams(
            host,
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let opened = daemon
        .ctx
        .floor_streams
        .as_ref()
        .unwrap()
        .subscribe()
        .unwrap();
    let frame: serde_json::Value =
        serde_json::from_slice(crate::floor_websocket::text_payload(&opened.snapshot).unwrap())
            .unwrap();
    assert_eq!(
        frame["projection"]["closing_time"],
        serde_json::json!({ "kind": "announced" })
    );
    assert_eq!(
        frame["projection"]["closing_progress"],
        serde_json::json!({ "acknowledged": 1, "settled": 0, "owed": 2, "running": 3, "remaining_ms": 0 })
    );
    let world = daemon.sup.world();
    assert_eq!(world.closing.as_ref().unwrap().participants.len(), 3);
    assert!(daemon.closing.expired(
        daemon.ctx.clock.now(),
        world.closing.as_ref().unwrap().announced_at
    ));
    let mut running = common::start(daemon);
    assert_eq!(running.join(), ExitCause::ShutdownRpc);
}

#[test]
fn historical_complete_boot_snapshot_has_no_closing_phase_or_progress() {
    let fixture = common::Fixture::new();
    let host = seed_closing(&fixture, true);
    let daemon = crate::boot(
        &settings(&fixture),
        common::seams(
            host,
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let opened = daemon
        .ctx
        .floor_streams
        .as_ref()
        .unwrap()
        .subscribe()
        .unwrap();
    let frame: serde_json::Value =
        serde_json::from_slice(crate::floor_websocket::text_payload(&opened.snapshot).unwrap())
            .unwrap();
    assert!(frame["projection"].get("closing_time").is_none());
    assert!(frame["projection"].get("closing_progress").is_none());
    let mut running = common::start(daemon);
    running.stop(ExitCause::ShutdownRpc);
}
