use super::work_test_support as common;
use super::*;
use crate::floor_stream::{FloorStreamFault, FloorStreams, QueueRead};
use crate::ports::{OpenedPty, PtyChild, PtyHostPort, PtyNativeBoundaryUnknown, PtyOpenFailure};
use dudes_proto::{InstructionBoundaryAbsenceProof, InstructionNativeBoundaryReceipt};
use dudes_proto::{
    InstructionGapKind, InstructionObservationOwner, InstructionSettlement,
    InstructionUnknownReason,
};
use dudes_providers::launch::{LaunchInput, LaunchSpec};
use dudes_supervisor::host::{AdoptionKey, NativeBoundaryReceipt};
use std::num::NonZeroUsize;
use std::sync::{Mutex, PoisonError};

type TestDaemon =
    Daemon<common::SharedHost, common::ScriptedRunner, common::TestClock, common::FixtureRng>;

#[derive(Clone)]
struct CloseHost(Arc<Mutex<CloseState>>);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReceiptAvailability {
    Available,
    Unavailable,
}

struct CloseState {
    pty: PtyId,
    pid: u32,
    token: NativeBoundaryReceipt,
    opened: bool,
    closed: bool,
    acknowledged: bool,
    acknowledgment: ReceiptAvailability,
    absence_query: ReceiptAvailability,
    queries: Vec<(PtyId, NativeBoundaryReceipt)>,
    close_calls: usize,
    calls: Vec<(PtyId, NativeBoundaryReceipt, bool)>,
    journal: PathBuf,
    other_pty: Option<PtyId>,
    other_close_calls: usize,
    shutdown_error: Option<&'static str>,
    shutdown_calls: usize,
    fail_refresh_on_close: Option<OnboardingFailure>,
}

impl PtyHostPort for CloseHost {
    fn open(&mut self, _: LaunchSpec, _: u16, _: u16) -> Result<OpenedPty, PtyOpenFailure> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (pty, pid) = if state.opened {
            (
                state.other_pty.expect("the second owned mock PTY"),
                state.pid + 1,
            )
        } else {
            state.opened = true;
            (state.pty, state.pid)
        };
        Ok(OpenedPty {
            pty,
            child: PtyChild {
                pid,
                #[cfg(windows)]
                job: None,
            },
        })
    }

    fn write(&mut self, _: PtyId, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("this fixture sends no runtime input"))
    }

    fn resize(&mut self, _: PtyId, _: u16, _: u16) -> io::Result<()> {
        Ok(())
    }

    fn subscribe(&mut self, _: PtyId) -> Option<crate::pty_output::OutputSubscription> {
        None
    }

    fn close(&mut self, pty: PtyId) -> PtyCloseOutcome {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.other_pty == Some(pty) {
            state.other_close_calls += 1;
            return PtyCloseOutcome::Failed(io::Error::other("the other target is unconfirmed"));
        }
        assert_eq!(pty, state.pty);
        assert!(state.opened);
        state.close_calls += 1;
        state.closed = true;
        if let Some(failure) = state.fail_refresh_on_close.take() {
            failure.fail_next_pump_spend_refresh();
        }
        PtyCloseOutcome::Closed
    }

    fn track_native_close(
        &mut self,
        pty: PtyId,
    ) -> Result<NativeBoundaryReceipt, PtyNativeBoundaryUnknown> {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if pty != state.pty || !state.opened || state.closed {
            return Err(PtyNativeBoundaryUnknown::Identity);
        }
        Ok(state.token.clone())
    }

    fn native_close_absence(
        &mut self,
        pty: PtyId,
        token: &NativeBoundaryReceipt,
    ) -> Result<(), PtyNativeBoundaryUnknown> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.queries.push((pty, token.clone()));
        if pty != state.pty || *token != state.token || state.acknowledged {
            return Err(PtyNativeBoundaryUnknown::Identity);
        }
        if !state.closed || state.absence_query == ReceiptAvailability::Unavailable {
            return Err(PtyNativeBoundaryUnknown::Unconfirmed);
        }
        Ok(())
    }

    fn acknowledge_native_close(&mut self, pty: PtyId, token: &NativeBoundaryReceipt) -> bool {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let durable = dudes_journal::Reader::new(&state.journal)
            .iter_from(Lsn::ZERO)
            .unwrap()
            .map(|item| item.unwrap())
            .filter(|(_, entry)| {
                matches!(
                    entry.record,
                    Record::InstructionAttemptBoundarySettled { .. }
                )
            })
            .count();
        assert_eq!(
            durable, 1,
            "native acknowledgment needs exactly one durable settlement"
        );
        let accepted = pty == state.pty
            && *token == state.token
            && state.closed
            && !state.acknowledged
            && state.acknowledgment == ReceiptAvailability::Available;
        state.calls.push((pty, token.clone(), accepted));
        state.acknowledged |= accepted;
        accepted
    }

    fn shutdown(&mut self) -> io::Result<()> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.shutdown_calls += 1;
        state
            .shutdown_error
            .map_or(Ok(()), |message| Err(io::Error::other(message)))
    }
}

fn append(daemon: &mut TestDaemon, record: Record) -> Lsn {
    let entry = Entry {
        at: daemon.ctx.clock.now(),
        record,
    };
    let append = daemon.ctx.append(daemon.sup.world(), &entry).unwrap();
    let lsn = append.lsn();
    let through = append.through();
    let (main, checkpoint) = append.into_parts();
    for (lsn, entry) in std::iter::once(main).chain(checkpoint) {
        daemon
            .sup
            .handle(
                Input::Replayed { lsn, entry },
                &dudes_supervisor::RecoveryOperation::empty(),
            )
            .unwrap();
    }
    assert_eq!(daemon.sup.world().last_applied_lsn, Some(through));
    assert!(daemon.sup.take_effects().is_empty());
    lsn
}

fn open_attached_interval(
    daemon: &mut TestDaemon,
    fixture: &common::Fixture,
    agent: AgentId,
    key: &AdoptionKey,
    expected_pty: PtyId,
) -> (dudes_proto::InstructionObservationOwner, Lsn, Lsn) {
    let attachment = append(
        daemon,
        Record::Attached {
            agent,
            by: dudes_proto::Actor::System,
            hook_port: None,
            provenance: None,
        },
    );
    let (owner, started) = daemon
        .ctx
        .start_attached_boundary(
            &mut daemon.sup,
            agent,
            fixture.state.path(),
            &VecDeque::new(),
            &mut daemon.closing,
        )
        .unwrap();
    assert!(
        matches!(owner, dudes_proto::InstructionObservationOwner::AttachedProcess {
    agent: actual_agent, attachment_lsn,
} if actual_agent == agent && attachment_lsn == attachment)
    );
    let (opened, _) = daemon
        .ctx
        .pty_host
        .open(
            LaunchSpec {
                argv: vec!["neutral-pty-fixture".into()],
                env: Vec::new(),
                cwd: fixture.state.path().into(),
                stdin: LaunchInput::Null,
            },
            80,
            24,
        )
        .unwrap();
    assert_eq!(opened, expected_pty);
    daemon
        .ctx
        .remember_attached_boundary(opened, owner, started, Some(key.clone()));
    daemon.ctx.live_ptys.insert(agent, opened);
    let spawn = append(
        daemon,
        Record::PtySpawned {
            agent,
            pty: opened,
            pid: key.pid,
            start_time: key.start_time,
            boot: key.boot.clone(),
        },
    );
    let recorded = daemon
        .sup
        .world()
        .attached_native_spawn_receipt(started)
        .unwrap();
    assert_eq!(recorded.owner, owner);
    assert_eq!(recorded.spawn_lsn, spawn);
    assert_eq!(recorded.key.pid, key.pid);
    assert_eq!(recorded.key.start_time, key.start_time);
    assert_eq!(recorded.key.boot, key.boot);
    (owner, started, spawn)
}

struct Attached {
    fixture: common::Fixture,
    daemon: TestDaemon,
    state: Arc<Mutex<CloseState>>,
    agent: AgentId,
    owner: dudes_proto::InstructionObservationOwner,
    started: Lsn,
    spawn: Lsn,
    token: NativeBoundaryReceipt,
}

impl Attached {
    fn new() -> Self {
        let fixture = common::Fixture::new();
        let pty = PtyId(43);
        let key = AdoptionKey {
            pid: 931,
            start_time: 901,
            boot: Some("mock-boot".into()),
        };
        let token = NativeBoundaryReceipt::fresh();
        let state = Arc::new(Mutex::new(CloseState {
            pty,
            pid: key.pid,
            token: token.clone(),
            opened: false,
            closed: false,
            acknowledged: false,
            acknowledgment: ReceiptAvailability::Unavailable,
            absence_query: ReceiptAvailability::Available,
            queries: Vec::new(),
            close_calls: 0,
            calls: Vec::new(),
            journal: fixture.journal_dir(),
            other_pty: None,
            other_close_calls: 0,
            shutdown_error: None,
            shutdown_calls: 0,
            fail_refresh_on_close: None,
        }));
        let settings = fixture.settings();
        let mut seams = common::seams(
            &settings,
            common::SharedHost::new(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        );
        seams.adapters = [Box::new(dudes_providers::claude::ClaudeAdapter::new(
            dudes_providers::claude::config::ClaudeConfig {
                command: "/usr/bin/true".into(),
                ..dudes_providers::claude::config::ClaudeConfig::default()
            },
        )) as Box<dyn dudes_providers::adapter::Adapter>]
        .into_iter()
        .collect();
        seams.pty_host = Box::new(CloseHost(Arc::clone(&state)));
        let mut daemon = crate::boot(&settings, seams).unwrap();
        let Answered::Hired(hired) = daemon
            .ask(Ask::Hire(Box::new(common::hire_params(
                fixture.state.path(),
            ))))
            .unwrap()
        else {
            panic!("the real hire caller must return its agent");
        };
        let (owner, started, spawn) =
            open_attached_interval(&mut daemon, &fixture, hired.agent, &key, pty);
        Self {
            fixture,
            daemon,
            state,
            agent: hired.agent,
            owner,
            started,
            spawn,
            token,
        }
    }
}

#[test]
fn a_durable_close_request_with_a_prefold_failure_fences_before_the_host_call() {
    let Attached {
        fixture,
        mut daemon,
        state,
        agent,
        ..
    } = Attached::new();
    let pty = state.lock().unwrap().pty;
    daemon.onboarding_failure().fail_next_pump_spend_refresh();
    let failure = daemon
        .pty_close(agent, pty, dudes_proto::Actor::System, None, "test close")
        .unwrap_err();
    assert!(
        failure
            .message
            .contains("injected pre-fold spend refresh failure")
    );
    let durable = fixture.entries();
    let (request_lsn, _) = durable.last().unwrap();
    assert!(matches!(durable.last().map(|(_, entry)| &entry.record),
        Some(Record::PtyCloseRequested { pty: requested, .. }) if *requested == pty));
    assert!(daemon.sup.world().last_applied_lsn < Some(*request_lsn));
    assert_eq!(state.lock().unwrap().close_calls, 0);
    assert!(
        daemon
            .ctx
            .cleanup_ptys
            .get(&pty)
            .is_none_or(|cleanup| !cleanup.pty_close_recorded)
    );
    assert!(daemon.ctx.journal_writes_stopped());
    daemon.attachment_lost(agent, pty, "canonical recovery precedes a physical close");
    assert_eq!(fixture.entries(), durable);
    assert_eq!(state.lock().unwrap().close_calls, 0);
}

#[test]
fn a_durable_close_with_a_prefold_refresh_failure_fences_later_writes() {
    let Attached {
        fixture,
        mut daemon,
        state,
        agent,
        started,
        ..
    } = Attached::new();
    let pty = state.lock().unwrap().pty;
    state.lock().unwrap().fail_refresh_on_close = Some(daemon.onboarding_failure());
    let failure = daemon
        .pty_close(agent, pty, dudes_proto::Actor::System, None, "test close")
        .unwrap_err();
    assert!(
        failure
            .message
            .contains("injected pre-fold spend refresh failure")
    );
    let durable = fixture.entries();
    let (closed_lsn, _) = durable.last().unwrap();
    assert!(matches!(
        durable.last().map(|(_, entry)| &entry.record),
        Some(Record::PtyCloseCompleted {
            outcome: dudes_proto::PtyCloseOutcome::Closed,
            ..
        })
    ));
    assert!(daemon.sup.world().last_applied_lsn < Some(*closed_lsn));
    assert!(daemon.ctx.cleanup_ptys[&pty].pty_close_recorded);
    assert!(
        daemon
            .sup
            .world()
            .instructions
            .attempt(started)
            .unwrap()
            .finished
            .is_none()
    );
    assert!(daemon.ctx.journal_writes_stopped());
    daemon.attachment_lost(agent, pty, "retry requires canonical recovery");
    assert_eq!(fixture.entries(), durable);
    assert_eq!(state.lock().unwrap().close_calls, 1);
    assert_eq!(daemon.ctx.live_ptys.get(&agent), Some(&pty));
}

#[test]
fn a_confirmed_shutdown_close_does_not_depend_on_another_targets_aggregate_failure() {
    let Attached {
        fixture,
        mut daemon,
        state,
        agent,
        ..
    } = Attached::new();
    let primary = state.lock().unwrap().pty;
    let other_pty = PtyId(primary.0 + 1);
    {
        let mut state = state.lock().unwrap();
        state.other_pty = Some(other_pty);
        state.shutdown_error = Some("the other shutdown target is unconfirmed");
        state.acknowledgment = ReceiptAvailability::Available;
    }
    let Answered::Hired(other) = daemon
        .ask(Ask::Hire(Box::new(common::hire_params(
            fixture.state.path(),
        ))))
        .unwrap()
    else {
        panic!("the other owned target must have its own agent");
    };
    let other_key = AdoptionKey {
        pid: state.lock().unwrap().pid + 1,
        start_time: 902,
        boot: Some("mock-boot".into()),
    };
    open_attached_interval(&mut daemon, &fixture, other.agent, &other_key, other_pty);
    daemon
        .append_failure()
        .fail_nth(NonZeroUsize::new(3).unwrap());
    assert!(!daemon.cleanup_all_ptys());
    assert_eq!(state.lock().unwrap().close_calls, 1);
    assert_eq!(state.lock().unwrap().other_close_calls, 1);
    assert_eq!(state.lock().unwrap().shutdown_calls, 1);
    assert_eq!(daemon.ctx.live_ptys.get(&other.agent), Some(&other_pty));
    assert!(!fixture.records().iter().any(|record| matches!(record,
        Record::Detached { agent: found, .. } if *found == other.agent)));
    assert!(!daemon.ctx.live_ptys.contains_key(&agent));
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record,
        Record::Detached { agent: found, .. } if *found == agent))
            .count(),
        1
    );
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record,
        Record::PtyCloseRequested { pty, .. } if *pty == primary))
            .count(),
        1
    );
}

#[test]
fn a_durable_close_survives_a_later_instruction_append_failure_without_reclosing() {
    let Attached {
        fixture,
        mut daemon,
        state,
        agent,
        owner,
        started,
        ..
    } = Attached::new();
    let pty = state.lock().unwrap().pty;
    daemon
        .append_failure()
        .fail_nth(NonZeroUsize::new(3).unwrap());
    assert!(
        daemon
            .pty_close(agent, pty, dudes_proto::Actor::System, None, "test close")
            .is_err()
    );
    let attempt = daemon.sup.world().instructions.attempt(started).unwrap();
    assert!(attempt.boundary_settled.is_none());
    assert!(fixture.records().iter().any(|record| matches!(
        record,
        Record::PtyCloseCompleted {
            outcome: dudes_proto::PtyCloseOutcome::Closed,
            ..
        }
    )));
    assert!(!fixture.records().iter().any(|record| matches!(record,
        Record::InstructionAttemptBoundarySettled { owner: found, .. } if *found == owner)));
    daemon.attachment_lost(agent, pty, "retry the failed instruction append");
    assert_eq!(
        state.lock().unwrap().close_calls,
        1,
        "the durable confirmed close must survive later fallible continuation"
    );
    assert!(fixture.records().iter().any(|record| matches!(record,
        Record::Detached { agent: found, .. } if *found == agent)));
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::PtyCloseCompleted { .. }))
            .count(),
        1
    );
}

#[test]
fn a_failed_completion_append_does_not_mark_a_confirmed_durable_close() {
    let Attached {
        fixture,
        mut daemon,
        state,
        agent,
        ..
    } = Attached::new();
    let pty = state.lock().unwrap().pty;
    daemon
        .append_failure()
        .fail_nth(NonZeroUsize::new(2).unwrap());
    assert!(
        daemon
            .pty_close(agent, pty, dudes_proto::Actor::System, None, "test close")
            .is_err()
    );
    assert_eq!(state.lock().unwrap().close_calls, 1);
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::PtyCloseCompleted { .. }))
    );
    assert!(
        daemon
            .ctx
            .cleanup_ptys
            .get(&pty)
            .is_none_or(|cleanup| !cleanup.pty_close_recorded)
    );
    daemon.attachment_lost(agent, pty, "retry an unrecorded close");
    assert_eq!(state.lock().unwrap().close_calls, 2);
    assert!(fixture.records().iter().any(|record| matches!(record,
        Record::Detached { agent: found, .. } if *found == agent)));
}

#[test]
fn a_keyed_attachment_without_an_end_does_not_use_keyless_uncertainty() {
    let Attached {
        fixture,
        mut daemon,
        state,
        agent,
        started,
        ..
    } = Attached::new();
    let before = fixture.entries();
    let pty = state.lock().unwrap().pty;
    assert!(
        daemon
            .ctx
            .finish_attached_boundary(&mut daemon.sup, agent, pty, &mut daemon.closing)
            .is_err()
    );
    assert_eq!(fixture.entries(), before);
    let attempt = daemon.sup.world().instructions.attempt(started).unwrap();
    assert!(attempt.spawn_lsn.is_some());
    assert!(attempt.finished.is_none());
    assert!(attempt.boundary_settled.is_none());
    assert_eq!(state.lock().unwrap().close_calls, 0);
}

#[test]
fn the_final_detached_append_failure_follows_completed_instruction_evidence() {
    let Attached {
        fixture,
        mut daemon,
        state,
        agent,
        ..
    } = Attached::new();
    let pty = state.lock().unwrap().pty;
    daemon
        .append_failure()
        .fail_nth(NonZeroUsize::new(5).unwrap());
    assert!(
        daemon
            .pty_close(agent, pty, dudes_proto::Actor::System, None, "test close")
            .is_err()
    );
    assert!(matches!(
        fixture.records().last(),
        Some(Record::InstructionObservationFinished { .. })
    ));
    assert!(!fixture.records().iter().any(|record| matches!(record,
        Record::Detached { agent: found, .. } if *found == agent)));
    assert_eq!(state.lock().unwrap().close_calls, 1);
    assert!(daemon.ctx.cleanup_ptys[&pty].pty_close_recorded);
    daemon.attachment_lost(agent, pty, "retry only final detachment");
    assert_eq!(state.lock().unwrap().close_calls, 1);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record,
            Record::Detached { agent: found, .. } if *found == agent))
            .count(),
        1
    );
}

#[test]
fn tick_retries_the_exact_attached_ack_after_its_spawn_index_has_retired() {
    let Attached {
        fixture,
        mut daemon,
        state,
        agent,
        owner,
        started,
        spawn,
        token,
    } = Attached::new();
    let pty = state.lock().unwrap().pty;

    daemon
        .pty_close(agent, pty, dudes_proto::Actor::System, None, "test close")
        .unwrap();
    let calls = state.lock().unwrap().calls.clone();
    assert!(!calls.is_empty());
    assert!(
        calls
            .iter()
            .all(|call| *call == (pty, token.clone(), false))
    );
    assert!(
        daemon
            .sup
            .world()
            .attached_native_spawn_receipt(started)
            .is_none()
    );
    let settled = daemon
        .sup
        .world()
        .instructions
        .attempt(started)
        .unwrap()
        .boundary_settled
        .as_ref()
        .unwrap();
    assert_eq!(
        settled.proof,
        InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
            receipt: InstructionNativeBoundaryReceipt::Spawned { lsn: spawn },
        }
    );
    let durable = fixture
        .entries()
        .into_iter()
        .filter(|(_, entry)| {
            matches!(entry.record,
        Record::InstructionAttemptBoundarySettled { owner: found, started_lsn, .. }
            if found == owner && started_lsn == started)
        })
        .collect::<Vec<_>>();
    assert_eq!(durable.len(), 1);
    assert_eq!(durable[0].0, settled.lsn);

    state.lock().unwrap().acknowledgment = ReceiptAvailability::Available;
    daemon.pump(Input::Tick).unwrap();
    let state = state.lock().unwrap();
    assert_eq!(
        state.calls.last(),
        Some(&(pty, token.clone(), true)),
        "the real pump must retry the retained exact acknowledgment after its live index is gone"
    );
    assert!(
        state
            .calls
            .iter()
            .all(|(called_pty, called_token, _)| *called_pty == pty && *called_token == token)
    );
    assert_eq!(
        state
            .calls
            .iter()
            .filter(|(_, _, accepted)| *accepted)
            .count(),
        1
    );
    assert!(state.acknowledged);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::InstructionAttemptBoundarySettled { .. }))
            .count(),
        1
    );
    let attempt = daemon.sup.world().instructions.attempt(started).unwrap();
    assert_eq!(attempt.owner, owner);
    assert_eq!(attempt.spawn_lsn, Some(spawn));
}

fn tick_after_unfolded_pause(daemon: &mut TestDaemon, agent: AgentId) {
    daemon
        .sup
        .handle(
            Input::Command(dudes_supervisor::Command::Pause {
                agent,
                browser: None,
            }),
            &dudes_supervisor::RecoveryOperation::empty(),
        )
        .unwrap();
    let mut queue = VecDeque::new();
    for effect in daemon.sup.take_effects() {
        daemon
            .ctx
            .execute(
                &effect,
                &mut queue,
                &mut PumpReport::Discard,
                &mut daemon.sup,
                &mut daemon.closing,
            )
            .unwrap();
    }
    assert!(!daemon.sup.world().agents[&agent].paused);
    assert!(
        queue
            .iter()
            .any(|input| matches!(input, Input::Persisted { .. }))
    );
    queue.push_back(Input::Tick);
    daemon
        .pump_inputs(
            &mut PumpReport::Discard,
            PumpOrigin::Ordinary,
            queue,
            &[],
            |_, _operation| Ok(Handled::default()),
        )
        .unwrap();
    assert!(
        daemon.sup.world().agents[&agent].paused,
        "native settlement cannot pass an earlier durable acknowledgment"
    );
}

#[test]
fn tick_retries_unavailable_attached_absence_after_comparison_and_detach() {
    let Attached {
        fixture,
        mut daemon,
        state,
        agent,
        owner,
        started,
        spawn,
        token,
    } = Attached::new();
    let pty = state.lock().unwrap().pty;
    let key = daemon
        .sup
        .world()
        .attached_native_spawn_receipt(started)
        .unwrap()
        .key
        .clone();
    state.lock().unwrap().absence_query = ReceiptAvailability::Unavailable;
    daemon
        .pty_close(
            agent,
            pty,
            dudes_proto::Actor::System,
            None,
            "query unavailable",
        )
        .unwrap();
    assert_comparison_detached_before_native_settlement(&daemon, &fixture, &state, agent, started);

    state.lock().unwrap().absence_query = ReceiptAvailability::Available;
    tick_after_unfolded_pause(&mut daemon, agent);
    let settled = daemon
        .sup
        .world()
        .instructions
        .attempt(started)
        .unwrap()
        .boundary_settled
        .as_ref()
        .expect("Tick must retry retained native query evidence even after comparison and Detach");
    assert_eq!(
        settled.proof,
        InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
            receipt: InstructionNativeBoundaryReceipt::Spawned { lsn: spawn },
        }
    );
    assert!(
        daemon
            .sup
            .world()
            .attached_native_spawn_receipt(started)
            .is_none()
    );
    let settlements: Vec<_> = fixture.entries().into_iter().filter(|(_, entry)| matches!(entry.record,
        Record::InstructionAttemptBoundarySettled { owner: found, started_lsn, .. } if found == owner && started_lsn == started)).collect();
    assert_eq!(settlements.len(), 1);
    assert_eq!(settlements[0].0, settled.lsn);
    assert!(
        state
            .lock()
            .unwrap()
            .calls
            .iter()
            .all(|call| *call == (pty, token.clone(), false))
    );

    state.lock().unwrap().acknowledgment = ReceiptAvailability::Available;
    daemon.pump(Input::Tick).unwrap();
    {
        let state = state.lock().unwrap();
        assert!(state.acknowledged);
        assert_eq!(state.calls.last(), Some(&(pty, token.clone(), true)));
        assert!(
            state
                .queries
                .iter()
                .all(|query| *query == (pty, token.clone()))
        );
        assert_eq!(state.close_calls, 1);
    }
    assert_original_attached_records(&fixture, agent, spawn, &key, pty);
    let attempt = daemon.sup.world().instructions.attempt(started).unwrap();
    assert_eq!(attempt.owner, owner);
    assert_eq!(attempt.spawn_lsn, Some(spawn));
}

fn published_floor_cuts(subscription: &crate::floor_stream::Subscription) -> Vec<Lsn> {
    let mut cuts = Vec::new();
    for _ in 0..256 {
        match subscription.next(Duration::ZERO) {
            QueueRead::Frame(bytes) => {
                let frame: serde_json::Value =
                    serde_json::from_slice(crate::floor_websocket::text_payload(&bytes).unwrap())
                        .unwrap();
                if let Some(lsn) = frame.get("delta").and_then(|delta| delta.get("lsn")) {
                    cuts.push(serde_json::from_value(lsn.clone()).unwrap());
                }
            }
            QueueRead::Empty => return cuts,
            QueueRead::Ended | QueueRead::Overflowed => {
                panic!("the actual Floor subscription must remain live")
            }
        }
    }
    panic!("the bounded fixture must drain all Floor publications");
}

#[test]
fn closing_attached_native_retry_publishes_its_folded_cut_without_fencing_floor_or_writer() {
    let Attached {
        fixture,
        mut daemon,
        state,
        agent,
        owner,
        started,
        ..
    } = Attached::new();
    let streams = FloorStreams::new(
        daemon.ctx.floor_snapshot().unwrap(),
        FloorStreamFault::default().without_heartbeat(),
    )
    .unwrap();
    daemon.ctx.floor_streams = Some(streams.clone());
    daemon
        .closing_request(crate::closing::Request::Announce)
        .unwrap();
    assert!(daemon.sup.is_closing());
    assert!(daemon.ctx.floor_closing.is_some());
    let opened = streams.subscribe().unwrap();
    let pty = state.lock().unwrap().pty;
    state.lock().unwrap().absence_query = ReceiptAvailability::Unavailable;
    let closed = daemon.pty_close(
        agent,
        pty,
        dudes_proto::Actor::System,
        None,
        "closing publication control",
    );
    assert!(
        closed.is_ok(),
        "normal close must publish its comparison cuts before Detach: {closed:?}"
    );
    assert!(!streams.is_ended());
    assert!(daemon.ctx.pending_floor.is_empty());
    assert!(daemon.ctx.pending_floor_base.is_none());
    let before = published_floor_cuts(&opened.subscription);
    assert!(!before.is_empty());
    assert!(before.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(
        daemon
            .sup
            .world()
            .instructions
            .attempt(started)
            .unwrap()
            .boundary_settled
            .is_none()
    );

    {
        let mut state = state.lock().unwrap();
        state.absence_query = ReceiptAvailability::Available;
        state.acknowledgment = ReceiptAvailability::Available;
    }
    let result = daemon.pump(Input::Tick);
    assert!(
        result.is_ok(),
        "the real closing pump must publish a direct native settlement fold: {result:?}"
    );
    assert!(
        !streams.is_ended(),
        "a folded native boundary must not end the Floor stream"
    );
    assert!(!daemon.ctx.journal_writes_stopped());
    assert!(daemon.ctx.closing_failed_after.is_none());
    assert!(daemon.ctx.pending_floor.is_empty());
    assert!(daemon.ctx.pending_floor_base.is_none());
    let settled = daemon
        .sup
        .world()
        .instructions
        .attempt(started)
        .unwrap()
        .boundary_settled
        .as_ref()
        .unwrap();
    assert!(
        fixture
            .entries()
            .iter()
            .any(|(lsn, entry)| *lsn == settled.lsn
                && matches!(entry.record,
        Record::InstructionAttemptBoundarySettled { owner: found, started_lsn, .. }
            if found == owner && started_lsn == started))
    );
    let after = published_floor_cuts(&opened.subscription);
    assert!(
        after.contains(&settled.lsn),
        "the exact durable settlement LSN needs a Floor delta"
    );
    assert!(after.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(before.last().unwrap() < after.first().unwrap());
    assert!(streams.subscribe().is_ok());
    assert!(state.lock().unwrap().acknowledged);
}

fn assert_comparison_detached_before_native_settlement(
    daemon: &TestDaemon,
    fixture: &common::Fixture,
    state: &Arc<Mutex<CloseState>>,
    agent: AgentId,
    started: Lsn,
) {
    let attempt = daemon.sup.world().instructions.attempt(started).unwrap();
    assert!(matches!(
        attempt
            .finished
            .as_ref()
            .map(|finished| &finished.settlement),
        Some(InstructionSettlement::Compared {
            end: dudes_proto::InstructionEndBoundary::AttachedProcess { .. },
            ..
        })
    ));
    assert!(attempt.boundary_settled.is_none());
    assert!(!daemon.sup.world().agents[&agent].attached);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(
                |record| matches!(record, Record::Detached { agent: found, .. } if *found == agent)
            )
            .count(),
        1
    );
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::InstructionAttemptBoundarySettled { .. }))
    );
    {
        let state = state.lock().unwrap();
        assert_eq!(state.close_calls, 1);
        assert!(!state.queries.is_empty());
        assert!(state.calls.is_empty());
    }
}

fn assert_original_attached_records(
    fixture: &common::Fixture,
    agent: AgentId,
    spawn: Lsn,
    key: &AdoptionKey,
    pty: PtyId,
) {
    let records = fixture.entries();
    assert_eq!(
        records
            .iter()
            .filter(|(_, entry)| matches!(
                entry.record,
                Record::InstructionAttemptBoundarySettled { .. }
            ))
            .count(),
        1
    );
    assert_eq!(records.iter().filter(|(_, entry)| matches!(entry.record, Record::Detached { agent: found, .. } if found == agent)).count(), 1);
    assert!(records.iter().any(|(lsn, entry)| *lsn == spawn && matches!(&entry.record,
        Record::PtySpawned { agent: found, pty: found_pty, pid, start_time, boot }
            if *found == agent && *found_pty == pty && *pid == key.pid && *start_time == key.start_time && *boot == key.boot)));
}

#[test]
fn shutdown_records_the_exact_attached_boundary_before_detaching_it() {
    let Attached {
        fixture,
        mut daemon,
        state,
        owner,
        started,
        spawn,
        ..
    } = Attached::new();
    state.lock().unwrap().acknowledgment = ReceiptAvailability::Available;
    assert!(daemon.cleanup_all_ptys());
    let records = fixture.records();
    let settled = records
        .iter()
        .position(|record| {
            matches!(record,
        Record::InstructionAttemptBoundarySettled { owner: found, started_lsn,
            proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                receipt: InstructionNativeBoundaryReceipt::Spawned { lsn } },
        } if *found == owner && *started_lsn == started && *lsn == spawn)
        })
        .expect("shutdown preserves the exact scoped absence before losing its capability");
    let detached = records
        .iter()
        .position(|record| matches!(record, Record::Detached { .. }))
        .unwrap();
    assert!(settled < detached);
    assert!(
        records[settled..detached]
            .iter()
            .any(|record| matches!(record,
        Record::InstructionObservationFinished { owner: found, .. } if *found == owner))
    );
    assert!(
        daemon
            .sup
            .world()
            .attached_native_spawn_receipt(started)
            .is_none()
    );
    assert!(state.lock().unwrap().acknowledged);
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::TurnIssued { .. }))
    );
}

#[derive(Clone, Copy)]
enum OpenFailure {
    Failed,
    CleanupUnconfirmed,
}

#[derive(Clone, Copy)]
struct CalledOpen {
    owner: InstructionObservationOwner,
    started: Lsn,
}

struct FailedOpenState {
    mode: OpenFailure,
    journal: PathBuf,
    called: Vec<CalledOpen>,
    append_failure: Option<AppendFailure>,
}

struct FailedOpenHost(Arc<Mutex<FailedOpenState>>);

impl PtyHostPort for FailedOpenHost {
    fn open(&mut self, _: LaunchSpec, _: u16, _: u16) -> Result<OpenedPty, PtyOpenFailure> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let called = dudes_journal::Reader::new(&state.journal)
            .iter_from(Lsn::ZERO)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter_map(|(started, entry)| match entry.record {
                Record::InstructionObservationStarted {
                    owner, snapshot, ..
                } => {
                    assert!(matches!(
                        owner,
                        InstructionObservationOwner::AttachedProcess { .. }
                    ));
                    snapshot.validate().unwrap();
                    Some(CalledOpen { owner, started })
                }
                _ => None,
            })
            .last()
            .expect("the real executor must fsync its exact baseline before calling the port");
        state.called.push(called);
        if let Some(failure) = state.append_failure.take() {
            failure.fail_nth(NonZeroUsize::new(1).unwrap());
        }
        Err(match state.mode {
            OpenFailure::Failed => {
                PtyOpenFailure::Failed(io::Error::other("mock failure after open was called"))
            }
            OpenFailure::CleanupUnconfirmed => PtyOpenFailure::CleanupUnconfirmed {
                pty: PtyId(91),
                cause: io::Error::other("mock failure after open was called"),
                cleanup: io::Error::other("mock cleanup remains unconfirmed"),
            },
        })
    }

    fn write(&mut self, _: PtyId, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other(
            "the failed-open fixture has no input channel",
        ))
    }

    fn resize(&mut self, _: PtyId, _: u16, _: u16) -> io::Result<()> {
        Err(io::Error::other("the failed-open fixture has no terminal"))
    }

    fn subscribe(&mut self, _: PtyId) -> Option<crate::pty_output::OutputSubscription> {
        None
    }

    fn close(&mut self, _: PtyId) -> PtyCloseOutcome {
        PtyCloseOutcome::Failed(io::Error::other("mock cleanup remains unconfirmed"))
    }

    fn shutdown(&mut self) -> io::Result<()> {
        Err(io::Error::other(
            "the mock supplies no native absence proof",
        ))
    }
}

struct FailedOpen {
    daemon: TestDaemon,
    fixture: common::Fixture,
    state: Arc<Mutex<FailedOpenState>>,
    agent: AgentId,
    effect: Effect,
}

impl FailedOpen {
    fn new(mode: OpenFailure, fail_settlement: bool) -> Self {
        let fixture = common::Fixture::new();
        let state = Arc::new(Mutex::new(FailedOpenState {
            mode,
            journal: fixture.journal_dir(),
            called: Vec::new(),
            append_failure: None,
        }));
        let settings = fixture.settings();
        let mut seams = common::seams(
            &settings,
            common::SharedHost::new(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        );
        seams.adapters = [Box::new(dudes_providers::claude::ClaudeAdapter::new(
            dudes_providers::claude::config::ClaudeConfig {
                command: "/usr/bin/true".into(),
                ..dudes_providers::claude::config::ClaudeConfig::default()
            },
        )) as Box<dyn dudes_providers::adapter::Adapter>]
        .into_iter()
        .collect();
        seams.pty_host = Box::new(FailedOpenHost(Arc::clone(&state)));
        let mut daemon = crate::boot(&settings, seams).unwrap();
        let Answered::Hired(hired) = daemon
            .ask(Ask::Hire(Box::new(common::hire_params(
                fixture.state.path(),
            ))))
            .unwrap()
        else {
            panic!("the real hire caller must return its agent");
        };
        append(
            &mut daemon,
            Record::Attached {
                agent: hired.agent,
                by: dudes_proto::Actor::System,
                hook_port: None,
                provenance: None,
            },
        );
        if fail_settlement {
            state.lock().unwrap().append_failure = Some(daemon.ctx.append_failure.clone());
        }
        let effect = Effect::OpenPty {
            agent: hired.agent,
            spec: LaunchSpec {
                argv: vec!["/usr/bin/true".into()],
                env: Vec::new(),
                cwd: fixture.state.path().into(),
                stdin: LaunchInput::Null,
            },
            install: dudes_providers::adapter::HookInstall::default(),
            cols: 80,
            rows: 24,
        };
        Self {
            daemon,
            fixture,
            state,
            agent: hired.agent,
            effect,
        }
    }

    fn execute(&mut self) -> Result<()> {
        let mut queue = VecDeque::new();
        let mut report = PumpReport::attach(self.agent);
        self.daemon.ctx.execute(
            &self.effect,
            &mut queue,
            &mut report,
            &mut self.daemon.sup,
            &mut self.daemon.closing,
        )?;
        self.daemon.pump_inputs(
            &mut report,
            PumpOrigin::Ordinary,
            queue,
            &[],
            |_, _operation| Ok(Handled::default()),
        )
    }

    fn called(&self) -> CalledOpen {
        let state = self.state.lock().unwrap();
        assert_eq!(
            state.called.len(),
            1,
            "another owned host call must remain refused"
        );
        state.called[0]
    }
}

#[test]
fn an_actual_failed_attached_open_settles_its_captured_attempt_as_unknown() {
    for (mode, reason, gap) in [
        (
            OpenFailure::Failed,
            InstructionUnknownReason::UnrecordedLaunchBoundary,
            InstructionGapKind::UnrecordedLaunchBoundary,
        ),
        (
            OpenFailure::CleanupUnconfirmed,
            InstructionUnknownReason::CleanupUnconfirmed,
            InstructionGapKind::CleanupUnconfirmed,
        ),
    ] {
        let mut owned = FailedOpen::new(mode, false);
        owned.execute().unwrap();
        let called = owned.called();
        let attempt = owned
            .daemon
            .sup
            .world()
            .instructions
            .attempt(called.started)
            .unwrap();
        assert_eq!(attempt.owner, called.owner);
        assert!(attempt.spawn_lsn.is_none());
        assert!(attempt.boundary_settled.is_none());
        assert!(
            matches!(attempt.finished.as_ref().map(|finished| &finished.settlement),
            Some(InstructionSettlement::Unknown { reason: actual, end: None, sampled_at: None,
                recovery: false, comparison }) if *actual == reason && comparison.changes.is_empty()
                && comparison.gaps.iter().any(|found| found.reason == gap)),
            "a called Attached open is uncertain even when no spawn key was returned"
        );
        assert!(!owned.fixture.records().iter().any(|record| matches!(
            record,
            Record::PtySpawned { .. }
                | Record::InstructionObservationFinished {
                    settlement: InstructionSettlement::NoProcess { .. },
                    ..
                }
        )));
    }
}

#[test]
fn failed_attached_open_settlement_append_retains_called_attempt_admission_refusal() {
    let mut owned = FailedOpen::new(OpenFailure::Failed, true);
    assert!(owned.execute().is_err());
    let called = owned.called();
    let attempt = owned
        .daemon
        .sup
        .world()
        .instructions
        .attempt(called.started)
        .unwrap();
    assert!(attempt.finished.is_none());
    assert!(attempt.spawn_lsn.is_none());
    assert!(
        matches!(owned.daemon.ctx.admit_prior_attempt(&owned.daemon.sup, owned.agent, None),
        Err(dudes_supervisor::SharedFolderRefusal::PriorAttemptUnresolved { agent }) if agent == owned.agent),
        "the native call happened, so its durable current baseline must never regain the uncalled exemption"
    );
    let mut queue = VecDeque::new();
    owned
        .daemon
        .ctx
        .execute(
            &owned.effect,
            &mut queue,
            &mut PumpReport::attach(owned.agent),
            &mut owned.daemon.sup,
            &mut owned.daemon.closing,
        )
        .unwrap();
    assert!(queue.iter().any(|input| matches!(input,
        Input::LaunchAdmissionRefused { agent, turn: None,
            refusal: dudes_supervisor::SharedFolderRefusal::PriorAttemptUnresolved { .. }, .. }
            if *agent == owned.agent)));
    assert_eq!(owned.called().started, called.started);
    assert!(!owned.fixture.records().iter().any(|record| matches!(
        record,
        Record::InstructionObservationFinished { .. } | Record::PtySpawned { .. }
    )));
}
