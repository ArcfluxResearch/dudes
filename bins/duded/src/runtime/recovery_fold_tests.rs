use super::work_test_support as common;
use super::*;

type TestDaemon =
    Daemon<common::SharedHost, common::ScriptedRunner, common::TestClock, common::FixtureRng>;

fn empty(fixture: &common::Fixture) -> TestDaemon {
    let settings = fixture.settings();
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
    daemon.pump(Input::Journaled { lsn, entry }).unwrap();
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
        let lsn = daemon.ctx.append(daemon.sup.world(), &entry).unwrap();
        let handled = daemon.sup.handle(Input::Replayed { lsn, entry }).unwrap();
        daemon
            .ctx
            .require_handled_through(daemon.sup.world(), &handled, lsn)
            .unwrap();
        let (handled, expected) = match mismatch {
            "none" => (
                daemon
                    .sup
                    .handle(Input::Spend {
                        agent,
                        view: SpendView::default(),
                    })
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
                let later = daemon.ctx.append(daemon.sup.world(), &entry).unwrap();
                daemon
                    .sup
                    .handle(Input::Replayed { lsn: later, entry })
                    .unwrap();
                assert!(later > lsn);
                (handled, lsn)
            }
            _ => unreachable!(),
        };
        let result = if mismatch == "advanced" {
            daemon.pump_started(&mut PumpReport::Discard, PumpOrigin::Ordinary, move |_| {
                Ok(handled)
            })
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
            |_| Ok(Handled::default()),
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
        .pump_started(&mut PumpReport::Discard, PumpOrigin::Closing, |_| {
            Err(anyhow::Error::new(
                dudes_supervisor::closing::ClosingError::LaunchPending,
            ))
        })
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
