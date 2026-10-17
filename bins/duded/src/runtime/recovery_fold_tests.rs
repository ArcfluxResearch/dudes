use super::work_test_support as common;
use super::*;

type TestDaemon =
    Daemon<common::SharedHost, common::ScriptedRunner, common::TestClock, common::FixtureRng>;

fn empty(fixture: &common::Fixture) -> TestDaemon {
    empty_with_checkpoints(fixture, None)
}

fn empty_with_checkpoints(fixture: &common::Fixture, cadence: Option<u64>) -> TestDaemon {
    let mut settings = fixture.settings();
    if let Some(cadence) = cadence {
        settings.journal.checkpoint_every = cadence;
    }
    crate::boot(
        &settings,
        common::seams(
            &settings,
            common::SharedHost::new(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap()
}

fn hire(daemon: &mut TestDaemon, fixture: &common::Fixture) -> AgentId {
    let Answered::Hired(result) = daemon
        .ask(Ask::Hire(Box::new(common::hire_params(
            fixture.state.path(),
        ))))
        .unwrap()
    else {
        panic!("hired")
    };
    result.agent
}

fn replay_append(daemon: &mut TestDaemon, append: RecordedAppend) -> Handled {
    let (main, checkpoint) = append.into_parts();
    let mut handled = daemon
        .sup
        .handle(
            Input::Replayed {
                lsn: main.0,
                entry: main.1,
            },
            &dudes_supervisor::RecoveryOperation::empty(),
        )
        .unwrap();
    if let Some((lsn, entry)) = checkpoint {
        handled = daemon
            .sup
            .handle(
                Input::Replayed { lsn, entry },
                &dudes_supervisor::RecoveryOperation::empty(),
            )
            .unwrap();
    }
    handled
}

#[test]
fn stale_journal_input_reports_no_fold_without_fencing_the_pump() {
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let agent = hire(&mut daemon, &fixture);
    let lsn = daemon.sup.world().last_applied_lsn.unwrap();
    let entry = Entry { at: Timestamp::UNIX_EPOCH, record: serde_json::from_value(serde_json::json!({
        "t":"mail", "id":dudes_types::MailId::from_parts(901, 1), "from":{"kind":"human"}, "to":{"kind":"agent","agent":agent},
        "act":"message", "subject":"stale", "body_len":0, "state":dudes_types::MailState::Durable
    })).unwrap() };
    let next = daemon.ctx.writer.next_lsn().unwrap();
    daemon
        .pump(Input::Journaled {
            append: dudes_supervisor::input::RecordedAppend::try_new(lsn, (lsn, entry), None)
                .unwrap(),
        })
        .unwrap();
    assert_eq!(daemon.sup.world().last_applied_lsn, Some(lsn));
    assert_eq!(daemon.ctx.writer.next_lsn().unwrap(), next);
    assert!(!daemon.ctx.journal_writes_stopped());
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn direct_and_pump_acknowledgments_require_some_and_the_exact_current_cut() {
    for mismatch in ["none", "expected", "advanced"] {
        let fixture = common::Fixture::new();
        let mut daemon = empty(&fixture);
        let agent = hire(&mut daemon, &fixture);
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::Pause {
                agent,
                browser: None,
            },
        };
        let append = daemon.ctx.append(daemon.sup.world(), &entry).unwrap();
        let lsn = append.through();
        let handled = replay_append(&mut daemon, append);
        daemon
            .ctx
            .require_handled_through(daemon.sup.world(), &handled, lsn)
            .unwrap();
        let (handled, expected) = match mismatch {
            "none" => (
                daemon
                    .sup
                    .handle(
                        Input::Spend {
                            agent,
                            view: SpendView::default(),
                        },
                        &dudes_supervisor::RecoveryOperation::empty(),
                    )
                    .unwrap(),
                lsn,
            ),
            "expected" => (handled, Lsn(lsn.0 + 1)),
            "advanced" => {
                let entry = Entry {
                    at: Timestamp::UNIX_EPOCH,
                    record: Record::Resume {
                        agent,
                        browser: None,
                    },
                };
                let append = daemon.ctx.append(daemon.sup.world(), &entry).unwrap();
                let later = append.through();
                replay_append(&mut daemon, append);
                assert!(later > lsn);
                (handled, lsn)
            }
            _ => unreachable!(),
        };
        let result = if mismatch == "advanced" {
            daemon.pump_started(
                &mut PumpReport::Discard,
                PumpOrigin::Ordinary,
                &[],
                move |_, _operation| Ok(handled),
            )
        } else {
            daemon
                .ctx
                .require_handled_through(daemon.sup.world(), &handled, expected)
        };
        assert!(result.is_err(), "{mismatch}");
        assert!(daemon.ctx.journal_writes_stopped(), "{mismatch}");
        assert!(daemon.sup.effects().is_empty());
        let mut running = common::start(daemon);
        running.stop(crate::ExitCause::Ended);
    }
}

#[test]
fn actual_world_refusal_fences_and_stops_later_queued_work() {
    use dudes_proto::recovery::{
        RecoveryBinding, RecoveryBoundary, RecoveryOrigin, RecoveryReason, RecoveryRequired,
        RecoveryWork,
    };
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let agent = hire(&mut daemon, &fixture);
    let owner = &daemon.sup.world().agents[&agent];
    let entry = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::RecoveryRequired(RecoveryRequired {
            binding: RecoveryBinding {
                work: RecoveryWork {
                    agent,
                    goal: GoalId::from_parts(902, 2),
                    turn: TurnId::from_parts(902, 3),
                },
                issue_lsn: Lsn(500),
                hired_lsn: owner.mcp.hire_lsn,
                ticket: None,
                worktree: None,
                provider: owner.provider,
                model_selection: None,
            },
            origin: RecoveryOrigin::ThisBarrier,
            reason: RecoveryReason::MissingSpawn,
            boundary: RecoveryBoundary::MissingSpawn,
            predecessor: None,
        }),
    };
    let (lsn, _) = daemon.ctx.writer.append_witnessed(&entry).unwrap();
    let before = daemon.sup.world().last_applied_lsn;
    let next = daemon.ctx.writer.next_lsn().unwrap();
    let queue = VecDeque::from([
        Input::Replayed { lsn, entry },
        Input::Command(Command::Pause {
            agent,
            browser: None,
        }),
    ]);
    let failure = daemon
        .pump_inputs(
            &mut PumpReport::Discard,
            PumpOrigin::Ordinary,
            queue,
            &[],
            |_, _operation| Ok(Handled::default()),
        )
        .unwrap_err();
    assert_eq!(
        failure.downcast_ref::<DurableInputError>(),
        Some(&DurableInputError::Recovery {
            lsn,
            reason: dudes_proto::recovery_state::RecoveryStateError::Binding
        })
    );
    assert!(daemon.ctx.journal_writes_stopped());
    assert_eq!(daemon.sup.world().last_applied_lsn, before);
    assert_eq!(daemon.ctx.writer.next_lsn().unwrap(), next);
    assert!(!daemon.sup.world().agents[&agent].paused);
    assert!(daemon.sup.effects().is_empty());
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn a_closing_start_refusal_keeps_its_original_error_identity() {
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let failure = daemon
        .pump_started(
            &mut PumpReport::Discard,
            PumpOrigin::Closing,
            &[],
            |_, _operation| {
                Err(anyhow::Error::new(
                    dudes_supervisor::closing::ClosingError::LaunchPending,
                ))
            },
        )
        .unwrap_err();
    assert_eq!(
        failure.downcast_ref::<dudes_supervisor::closing::ClosingError>(),
        Some(&dudes_supervisor::closing::ClosingError::LaunchPending)
    );
    assert!(!daemon.ctx.journal_writes_stopped());
    assert!(daemon.sup.effects().is_empty());
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}

fn append_replayed(daemon: &mut TestDaemon, record: Record) -> Lsn {
    let entry = Entry {
        at: daemon.ctx.clock.now(),
        record,
    };
    let append = daemon.ctx.append(daemon.sup.world(), &entry).unwrap();
    let lsn = append.lsn();
    let through = append.through();
    let handled = replay_append(daemon, append);
    assert_eq!(handled.folded_through(), Some(through));
    lsn
}

fn ordinary_owed(
    daemon: &mut TestDaemon,
    agent: AgentId,
) -> dudes_proto::recovery::RecoveryBinding {
    let mut installed = dudes_proto::record::samples::record(dudes_proto::RecordKind::GoalSet);
    let Record::GoalSet {
        agent: owner, goal, ..
    } = &mut installed
    else {
        unreachable!()
    };
    *owner = agent;
    let goal = goal.id;
    append_replayed(daemon, installed);
    let task = "unchanged owed task".to_owned();
    let decision = append_replayed(
        daemon,
        Record::LoopDecided {
            agent,
            input_digest: "ab".repeat(32),
            decision: dudes_proto::LoopDecision::Continue { task: task.clone() },
            gate_trace: Vec::new(),
        },
    );
    let turn = TurnId::from_parts(904, 3);
    let selection = Some(dudes_types::ModelSelection::provider_default());
    let issue_lsn = append_replayed(
        daemon,
        Record::TurnIssued {
            agent,
            goal,
            turn,
            mode: Mode::Managed,
            task,
            session: None,
            model_selection: selection.clone(),
            lsn_of_decision: decision,
            answers: Vec::new(),
        },
    );
    let row = daemon.ctx.projections.agent(agent).unwrap().unwrap();
    dudes_proto::recovery::RecoveryBinding {
        work: dudes_proto::recovery::RecoveryWork { agent, goal, turn },
        issue_lsn,
        hired_lsn: row.hired_lsn,
        ticket: None,
        worktree: row.worktree,
        provider: row.provider,
        model_selection: selection,
    }
}

#[test]
fn checkpointed_projection_only_records_do_not_create_live_acknowledgments() {
    use dudes_proto::recovery::{
        RecoveryBoundary, RecoveryOrigin, RecoveryReason, RecoveryRequired,
    };
    let fixture = common::Fixture::new();
    let mut daemon = empty_with_checkpoints(&fixture, Some(1));
    let agent = hire(&mut daemon, &fixture);
    let binding = ordinary_owed(&mut daemon, agent);
    let before = daemon.sup.world().last_applied_lsn;
    let prepared = daemon
        .ctx
        .memory
        .prepare_append(
            agent,
            b"bounded note",
            dudes_proto::PeerProvenance::Agent(agent),
        )
        .unwrap();
    let memory = daemon
        .append_memory_entry(prepared.record().clone(), "append")
        .unwrap();
    let pty = PtyId(904);
    let child = daemon
        .append_pty_entry(
            Record::PtySpawned {
                agent,
                pty,
                pid: 42,
                start_time: 7,
                boot: None,
            },
            "record PTY",
        )
        .unwrap();
    assert!(memory < child);
    assert_eq!(daemon.sup.world().last_applied_lsn, before);
    assert!(daemon.ctx.projections.last_applied_lsn() > child);
    let required = Entry {
        at: daemon.ctx.clock.now(),
        record: Record::RecoveryRequired(RecoveryRequired {
            binding,
            origin: RecoveryOrigin::ThisBarrier,
            reason: RecoveryReason::MissingSpawn,
            boundary: RecoveryBoundary::MissingSpawn,
            predecessor: None,
        }),
    };
    let append = daemon.ctx.append(daemon.sup.world(), &required).unwrap();
    let main = append.lsn();
    let through = append.through();
    assert!(through > main && main > child);
    let handled = replay_append(&mut daemon, append);
    daemon
        .ctx
        .require_handled_through(daemon.sup.world(), &handled, through)
        .unwrap();
    let mut canonical = dudes_supervisor::World::default();
    for (lsn, entry) in fixture.entries() {
        canonical.apply_checked(lsn, &entry).unwrap();
    }
    assert_eq!(
        daemon.sup.world().recovery_projection().unwrap(),
        canonical.recovery_projection().unwrap()
    );
    let mut persistent = Projections::in_memory().unwrap();
    dudes_journal::replay(&daemon.ctx.writer.reader(), &mut persistent).unwrap();
    assert_eq!(
        daemon.ctx.projections.export().unwrap(),
        persistent.export().unwrap()
    );
    assert_shutdown_detach_replays_before_started(daemon, &fixture, agent, pty);
}

fn assert_shutdown_detach_replays_before_started(
    mut daemon: TestDaemon,
    fixture: &common::Fixture,
    agent: AgentId,
    pty: PtyId,
) {
    append_replayed(
        &mut daemon,
        Record::Attached {
            agent,
            by: Actor::System,
            hook_port: None,
            provenance: None,
        },
    );
    assert!(daemon.sup.world().agents[&agent].attached);
    let cut = daemon.sup.world().last_applied_lsn;
    assert!(daemon.detach_during_shutdown(agent, pty));
    assert_eq!(daemon.sup.world().last_applied_lsn, cut);
    assert!(daemon.sup.world().agents[&agent].attached);
    assert!(matches!(
        fixture.records().last(),
        Some(Record::Checkpoint { .. })
    ));
    assert!(
        fixture.records().iter().any(
            |record| matches!(record, Record::Detached { agent: owner, .. } if *owner == agent)
        )
    );
    let detached_cut = daemon.ctx.projections.last_applied_lsn();
    drop(daemon);
    let (reader, _) = dudes_journal::Reader::open(&fixture.journal_dir()).unwrap();
    let mut replayed = dudes_supervisor::World::default();
    for item in reader.iter_from(Lsn::ZERO).unwrap() {
        let (lsn, entry) = item.unwrap();
        replayed.apply_checked(lsn, &entry).unwrap();
    }
    assert_eq!(replayed.last_applied_lsn, Some(detached_cut));
    assert!(
        !replayed.agents[&agent].attached,
        "Reader replay clears attachment before Started work"
    );
    let mut after = Projections::in_memory().unwrap();
    dudes_journal::replay(&reader, &mut after).unwrap();
    assert_eq!(after.last_applied_lsn(), detached_cut);
}

#[test]
fn checkpointed_live_and_direct_boundaries_reach_the_actual_final_cut() {
    let fixture = common::Fixture::new();
    let mut daemon = empty_with_checkpoints(&fixture, Some(1));
    let agent = hire(&mut daemon, &fixture);
    daemon
        .command(Command::Pause {
            agent,
            browser: None,
        })
        .unwrap();
    assert!(daemon.sup.world().agents[&agent].paused);
    assert_eq!(
        daemon.sup.world().last_applied_lsn,
        Some(daemon.ctx.projections.last_applied_lsn())
    );
    let close = daemon
        .append_pty_entry(
            Record::PtyCloseRequested {
                pty: PtyId(905),
                actor: Actor::System,
                provenance: None,
                reason: "checkpointed boundary".into(),
            },
            "record checkpointed PTY close",
        )
        .unwrap();
    let through = daemon.ctx.projections.last_applied_lsn();
    assert!(through > close);
    assert_eq!(daemon.sup.world().last_applied_lsn, Some(through));
    assert!(
        fixture.entries().iter().any(|(lsn, entry)| *lsn == close
            && matches!(entry.record, Record::PtyCloseRequested { .. }))
    );
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}
