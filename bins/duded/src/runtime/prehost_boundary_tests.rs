use super::work_test_support as common;
use super::*;
use dudes_proto::{
    AutomaticHold, InstructionBaselineRef, InstructionBoundaryAbsenceProof, InstructionEndBoundary,
    InstructionIssueAssociation, InstructionNativeBoundaryReceipt, InstructionNoProcessProof,
    InstructionObservationOwner, InstructionSettlement, InstructionTicketAssociation,
    InstructionUnknownReason, TurnOutcome,
};
use dudes_providers::adapter::{HookInstall, ManagedInstall};
use dudes_providers::launch::LaunchSpec;
use dudes_supervisor::host::{AdoptionKey, HostBoot, Liveness};
use dudes_types::Provenance;

type TestDaemon =
    Daemon<CountingHost, common::ScriptedRunner, common::TestClock, common::FixtureRng>;

#[derive(Clone)]
struct CountingHost {
    inner: common::SharedHost,
    calls: Arc<AtomicUsize>,
}

impl CountingHost {
    fn new() -> Self {
        Self {
            inner: common::SharedHost::new(),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn refusing() -> Self {
        Self {
            inner: common::SharedHost::with(dudes_supervisor::testing::FakeHost {
                fail_spawn: Some(io::ErrorKind::PermissionDenied),
                ..dudes_supervisor::testing::FakeHost::wall_clock_anchored()
            }),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn spawn_calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ProcessHost for CountingHost {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.spawn(spec, install)
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.inner.kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.inner.alive(key)
    }

    fn boot_identity(&self) -> HostBoot {
        self.inner.boot_identity()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown()
    }

    fn write_hooks(&mut self, install: &HookInstall) -> io::Result<()> {
        self.inner.write_hooks(install)
    }
}

struct Prepared {
    agent: AgentId,
    turn: TurnId,
    started: Lsn,
    owner: InstructionObservationOwner,
    ticket: Option<InstructionTicketAssociation>,
    issue: Lsn,
    task: String,
    spawn: Effect,
}

fn boot(
    fixture: &common::Fixture,
    host: &CountingHost,
    provider: Provider,
    command: &str,
) -> TestDaemon {
    let settings = fixture.settings();
    let mut seams = common::seams(
        &settings,
        host.clone(),
        Arc::new(common::ScriptedRunner::default()),
        Arc::new(common::TestClock::new()),
        Witness::default(),
    );
    let adapter: Box<dyn dudes_providers::adapter::Adapter> = match provider {
        Provider::Claude => Box::new(dudes_providers::claude::ClaudeAdapter::new(
            dudes_providers::claude::config::ClaudeConfig {
                command: command.into(),
                ..dudes_providers::claude::config::ClaudeConfig::default()
            },
        )),
        Provider::Codex => Box::new(dudes_providers::codex::CodexAdapter::new(
            dudes_providers::codex::config::CodexConfig {
                command: command.to_owned(),
                streaming: false,
            },
        )),
        _ => panic!("the pre-host fixtures use Claude or legacy Codex"),
    };
    seams.adapters = [adapter].into_iter().collect();
    crate::boot(&settings, seams).expect("real pre-host fixture boot")
}

fn execute(daemon: &mut TestDaemon, effect: &Effect, queue: &mut VecDeque<Input>) {
    daemon
        .ctx
        .execute(
            effect,
            queue,
            &mut PumpReport::Discard,
            &mut daemon.sup,
            &mut daemon.closing,
        )
        .expect("real pre-host persistence and launch executor");
}

fn next_input(daemon: &TestDaemon, queue: &mut VecDeque<Input>) -> Input {
    queue
        .pop_front()
        .unwrap_or_else(|| match daemon.jobs.recv_timeout(Duration::from_secs(5)) {
            Ok(Job::Input(input)) => *input,
            _ => panic!("bounded pre-host fixture awaits an actual worker input"),
        })
}

fn prepared(daemon: &mut TestDaemon, fixture: &common::Fixture, provider: Provider) -> Prepared {
    let mut hire = common::hire_params(fixture.state.path());
    hire.provider = provider;
    let Answered::Hired(hired) = daemon
        .ask(Ask::Hire(Box::new(hire)))
        .expect("hire before the real owned launch")
    else {
        panic!("expected a hired managed agent")
    };
    let agent = hired.agent;
    let params = common::goal_params(agent, &[common::true_program()]);
    let mut queue = VecDeque::from([Input::Command(Command::GoalSet {
        agent,
        text: params.text,
        predicate: params.predicate,
        budget: params.budget,
        hop_cap: params.hop_cap,
        browser_goal: None,
        goal_replacement: None,
        completion_check: None,
        baseline: None,
    })]);
    for _ in 0..64 {
        let input = next_input(daemon, &mut queue);
        daemon.sup.handle(input);
        for effect in daemon.sup.take_effects() {
            let Effect::Spawn { turn, .. } = &effect else {
                execute(daemon, &effect, &mut queue);
                continue;
            };
            let turn = *turn;
            let (started, attempt) = daemon
                .sup
                .world()
                .instructions
                .current_for_turn(turn)
                .expect("real folded starting observation before launch");
            let prepared = Prepared {
                agent,
                turn,
                started,
                owner: attempt.owner,
                ticket: attempt.ticket,
                issue: daemon
                    .sup
                    .world()
                    .instructions
                    .issue_lsn(turn)
                    .expect("real folded issue before launch"),
                task: daemon.sup.world().turns[&turn].task.clone(),
                spawn: effect,
            };
            assert_eq!(
                prepared.owner,
                InstructionObservationOwner::Managed {
                    agent,
                    turn,
                    issue: InstructionIssueAssociation::Prospective,
                }
            );
            assert!(prepared.started < prepared.issue);
            assert!(fixture.entries().iter().any(|(lsn, entry)| {
                *lsn == prepared.started
                    && matches!(entry.record,
                        Record::InstructionObservationStarted { owner, ticket, .. }
                            if owner == prepared.owner && ticket == prepared.ticket)
            }));
            assert!(fixture.entries().iter().any(|(lsn, entry)| {
                *lsn == prepared.issue
                    && matches!(&entry.record,
                        Record::TurnIssued { agent: issued_agent, turn: issued_turn, task, .. }
                            if *issued_agent == agent && *issued_turn == turn && *task == prepared.task)
            }));
            return prepared;
        }
    }
    panic!("bounded real preparation did not produce an owned Spawn effect")
}

fn ordinary_failure_folded(daemon: &TestDaemon, attempt: &Prepared) -> bool {
    daemon.sup.world().turns[&attempt.turn].has_ended()
        && matches!(
            daemon.sup.world().agents[&attempt.agent].automatic_hold,
            Some(AutomaticHold::LaunchFailed { .. })
        )
}

fn failed_end(daemon: &mut TestDaemon, attempt: &Prepared, queue: &mut VecDeque<Input>) -> Effect {
    for _ in 0..64 {
        let input = next_input(daemon, queue);
        daemon.sup.handle(input);
        let mut end = None;
        for effect in daemon.sup.take_effects() {
            if matches!(&effect,
                Effect::Persist { entry, .. }
                    if matches!(entry.record,
                        Record::TurnEnded {
                            agent,
                            turn,
                            mode: Mode::Managed,
                            provenance: Provenance::Observed,
                            outcome: Some(TurnOutcome::NotLaunched { .. }),
                            ..
                        } if agent == attempt.agent && turn == attempt.turn))
            {
                assert!(end.is_none(), "only one observed failed end is emitted");
                end = Some(effect);
            } else {
                execute(daemon, &effect, queue);
            }
        }
        if let Some(end) = end {
            return end;
        }
    }
    panic!("bounded failure folding did not produce its actual NotLaunched end")
}

fn no_host_finish(record: &Record, attempt: &Prepared, proof_lsn: Option<Lsn>) -> bool {
    matches!(record,
        Record::InstructionObservationFinished {
            owner,
            ticket,
            baseline: InstructionBaselineRef::Started { lsn },
            settlement: InstructionSettlement::NoProcess {
                proof_lsn: proof,
                proof: InstructionNoProcessProof::NoHostCall,
            },
        } if *owner == attempt.owner && *ticket == attempt.ticket && *lsn == attempt.started
            && proof_lsn.is_none_or(|expected| *proof == expected))
}

fn persist_failed_end(
    daemon: &mut TestDaemon,
    fixture: &common::Fixture,
    attempt: &Prepared,
    queue: &mut VecDeque<Input>,
) -> (Lsn, Input) {
    let end = failed_end(daemon, attempt, queue);
    let Effect::Persist { token, entry } = &end else {
        unreachable!()
    };
    let end_token = *token;
    let end_record = entry.record.clone();
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| no_host_finish(record, attempt, None))
    );
    execute(daemon, &end, queue);
    let end_lsn = fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| (entry.record == end_record).then_some(lsn))
        .expect("the full-fsync journal already contains the actual NotLaunched end");
    assert!(end_lsn > attempt.issue);
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| no_host_finish(record, attempt, None)),
        "durable end alone cannot produce a no-host settlement"
    );
    assert!(
        daemon
            .sup
            .world()
            .instructions
            .attempt(attempt.started)
            .unwrap()
            .finished
            .is_none(),
        "the end's acknowledgement has not reached the World fold"
    );
    assert!(daemon.sup.world().turns[&attempt.turn].is_issued_not_spawned());
    let ack = queue
        .iter()
        .position(|input| {
            matches!(input,
            Input::Persisted { token, lsn } if *token == end_token && *lsn == end_lsn)
        })
        .and_then(|position| queue.remove(position))
        .expect("real post-fsync acknowledgement of this exact end");
    (end_lsn, ack)
}

fn fold_failure(daemon: &mut TestDaemon, attempt: &Prepared, queue: &mut VecDeque<Input>) {
    for _ in 0..64 {
        if queue.is_empty() && ordinary_failure_folded(daemon, attempt) {
            break;
        }
        let input = next_input(daemon, queue);
        daemon.sup.handle(input);
        for effect in daemon.sup.take_effects() {
            execute(daemon, &effect, queue);
        }
    }
    assert!(
        queue.is_empty(),
        "bounded failure folding drains acknowledgements"
    );
    assert!(ordinary_failure_folded(daemon, attempt));
}

fn assert_prehost_failure(
    daemon: &mut TestDaemon,
    fixture: &common::Fixture,
    host: &CountingHost,
    attempt: &Prepared,
) {
    let mut queue = VecDeque::new();
    execute(daemon, &attempt.spawn, &mut queue);
    assert_eq!(host.spawn_calls(), 0, "preparation never calls the host");
    let (end_lsn, ack) = persist_failed_end(daemon, fixture, attempt, &mut queue);
    daemon.sup.handle(ack);
    let effects = daemon.sup.take_effects();
    assert!(
        effects.iter().any(|effect| matches!(effect,
        Effect::Persist { entry, .. } if no_host_finish(&entry.record, attempt, Some(end_lsn)))),
        "the exact end's folded acknowledgement continues the live zero-host-call assertion"
    );
    for effect in effects {
        execute(daemon, &effect, &mut queue);
    }
    fold_failure(daemon, attempt, &mut queue);
    assert_eq!(
        host.spawn_calls(),
        0,
        "no later failure handling calls the host"
    );
    assert_failed_turn(daemon, fixture, attempt);
    assert_no_host_settlement(daemon, fixture, attempt, end_lsn);
}

fn assert_failed_turn(daemon: &TestDaemon, fixture: &common::Fixture, attempt: &Prepared) {
    assert_eq!(daemon.sup.world().turns[&attempt.turn].task, attempt.task);
    assert_eq!(
        daemon.sup.world().instructions.issue_lsn(attempt.turn),
        Some(attempt.issue)
    );
    assert_eq!(daemon.sup.world().agents[&attempt.agent].open_turn, None);
    let entries = fixture.entries();
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry.record,
            Record::TurnIssued { turn, .. } if turn == attempt.turn))
            .count(),
        1,
        "the original immutable issue is retained"
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry.record,
            Record::TurnEnded {
                agent,
                turn,
                mode: Mode::Managed,
                provenance: Provenance::Observed,
                outcome: Some(TurnOutcome::NotLaunched { .. }),
                ..
            } if agent == attempt.agent && turn == attempt.turn))
            .count(),
        1,
        "ordinary failed-turn behavior is preserved for the original TurnId"
    );
    assert!(
        !entries.iter().any(|(_, entry)| matches!(entry.record,
        Record::AgentHeld { agent, cause: AutomaticHold::LaunchRefused { .. }, .. }
            if agent == attempt.agent)),
        "ordinary failure needs no synthetic refusal hold"
    );
}

fn assert_no_host_settlement(
    daemon: &TestDaemon,
    fixture: &common::Fixture,
    attempt: &Prepared,
    end_lsn: Lsn,
) {
    let entries = fixture.entries();
    let finishes: Vec<_> = entries
        .iter()
        .filter(|(_, entry)| no_host_finish(&entry.record, attempt, Some(end_lsn)))
        .collect();
    assert_eq!(
        finishes.len(),
        1,
        "the captured physical attempt settles once"
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry.record,
            Record::InstructionObservationFinished {
                baseline: InstructionBaselineRef::Started { lsn },
                ..
            } if lsn == attempt.started))
            .count(),
        1,
        "no competing terminal observation is written for this attempt"
    );
    assert!(finishes[0].0 > end_lsn);
    let folded = daemon
        .sup
        .world()
        .instructions
        .attempt(attempt.started)
        .unwrap();
    assert!(
        matches!(folded.finished.as_ref().map(|finish| &finish.settlement),
        Some(InstructionSettlement::NoProcess {
            proof_lsn,
            proof: InstructionNoProcessProof::NoHostCall,
        }) if *proof_lsn == end_lsn)
    );
    assert!(folded.boundary_settled.is_none());
    assert!(!entries.iter().any(|(_, entry)| matches!(entry.record,
        Record::InstructionAttemptBoundarySettled { started_lsn, .. } if started_lsn == attempt.started)));
    assert_eq!(
        daemon
            .sup
            .world()
            .instructions
            .unresolved_launch_attempts(attempt.agent)
            .count(),
        0
    );
}

#[test]
fn program_resolution_failure_has_a_durable_no_host_proof_and_preserves_failed_turn_behavior() {
    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let missing = fixture.runtime.path().join("missing-managed-program");
    assert!(!missing.exists());
    let mut daemon = boot(
        &fixture,
        &host,
        Provider::Claude,
        missing.to_str().expect("fixture program path"),
    );
    let attempt = prepared(&mut daemon, &fixture, Provider::Claude);
    assert_prehost_failure(&mut daemon, &fixture, &host, &attempt);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn child_preparation_failure_has_a_durable_no_host_proof_and_preserves_failed_turn_behavior() {
    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let mut daemon = boot(&fixture, &host, Provider::Codex, common::true_program());
    let mut attempt = prepared(&mut daemon, &fixture, Provider::Codex);
    let Effect::Spawn { spec, .. } = &mut attempt.spawn else {
        unreachable!()
    };
    assert_eq!(spec.argv.get(2), Some(&OsString::from("--add-dir")));
    spec.argv[2] = "--missing-owned-outbox-option".into();
    crate::program::resolve_launch(spec.clone())
        .expect("this fixture reaches child preparation after successful program resolution");
    assert_prehost_failure(&mut daemon, &fixture, &host, &attempt);
}

#[test]
fn an_actual_host_not_launched_end_does_not_become_a_no_host_call_assertion() {
    let fixture = common::Fixture::new();
    let host = CountingHost::refusing();
    let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
    let attempt = prepared(&mut daemon, &fixture, Provider::Claude);
    let mut queue = VecDeque::new();
    execute(&mut daemon, &attempt.spawn, &mut queue);
    assert_eq!(
        host.spawn_calls(),
        1,
        "the host itself answered NotLaunched"
    );
    let (end_lsn, ack) = persist_failed_end(&mut daemon, &fixture, &attempt, &mut queue);
    daemon.sup.handle(ack);
    let effects = daemon.sup.take_effects();
    assert!(
        !effects.iter().any(|effect| matches!(effect,
        Effect::Persist { entry, .. } if no_host_finish(&entry.record, &attempt, None))),
        "an end acknowledgement cannot reconstruct a zero-host-call assertion"
    );
    for effect in effects {
        execute(&mut daemon, &effect, &mut queue);
    }
    fold_failure(&mut daemon, &attempt, &mut queue);
    assert_eq!(host.spawn_calls(), 1);
    assert_failed_turn(&daemon, &fixture, &attempt);
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| no_host_finish(record, &attempt, None))
    );
    let folded = daemon
        .sup
        .world()
        .instructions
        .attempt(attempt.started)
        .unwrap();
    assert!(
        matches!(folded.finished.as_ref().map(|finish| &finish.settlement),
        Some(InstructionSettlement::Compared {
            end: InstructionEndBoundary::ManagedTurn { turn, end_lsn: ended },
            ..
        } | InstructionSettlement::Unknown {
            end: Some(InstructionEndBoundary::ManagedTurn { turn, end_lsn: ended }),
            ..
        }) if *turn == attempt.turn && *ended == end_lsn)
    );
    assert!(
        daemon
            .ctx
            .settle_one_native_boundary(&daemon.sup, &mut queue)
            .expect("the actual host failure retains its native absence evidence")
    );
    let native = queue
        .pop_front()
        .expect("actual journaled native boundary input");
    assert!(matches!(&native,
        Input::InstructionJournaled { entry, .. }
            if matches!(entry.record,
                Record::InstructionAttemptBoundarySettled {
                    owner,
                    started_lsn,
                    proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                        receipt: InstructionNativeBoundaryReceipt::LaunchFailureEnded { lsn },
                    },
                } if owner == attempt.owner && started_lsn == attempt.started && lsn == end_lsn)));
    daemon.sup.handle(native);
    assert!(daemon.sup.take_effects().is_empty());
    assert!(queue.is_empty());
    assert_eq!(host.spawn_calls(), 1);
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| no_host_finish(record, &attempt, None))
    );
}

#[test]
fn reboot_after_only_a_durable_hold_retains_uncertainty_instead_of_reconstructing_no_host_proof() {
    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
    let attempt = prepared(&mut daemon, &fixture, Provider::Claude);
    let hold = Entry {
        at: daemon.ctx.clock.now(),
        record: Record::AgentHeld {
            agent: attempt.agent,
            provenance: Provenance::Observed,
            cause: AutomaticHold::LaunchRefused {
                reason: "fixture crash after hold and before no-host acknowledgement".to_owned(),
            },
        },
    };
    let hold_lsn = daemon
        .ctx
        .append(daemon.sup.world(), &hold)
        .expect("real full-fsync hold append before crash");
    assert!(hold_lsn > attempt.issue);
    assert!(
        fixture
            .entries()
            .iter()
            .any(|(lsn, entry)| *lsn == hold_lsn && *entry == hold)
    );
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| no_host_finish(record, &attempt, None))
    );
    drop(daemon);
    let resumed = boot(&fixture, &host, Provider::Claude, common::true_program());
    assert_eq!(host.spawn_calls(), 0);
    assert_eq!(resumed.sup.world().turns[&attempt.turn].task, attempt.task);
    assert_eq!(
        resumed.sup.world().agents[&attempt.agent].open_turn,
        Some(attempt.turn)
    );
    assert_eq!(
        resumed.sup.world().instructions.issue_lsn(attempt.turn),
        Some(attempt.issue)
    );
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| no_host_finish(record, &attempt, None))
    );
    let folded = resumed
        .sup
        .world()
        .instructions
        .attempt(attempt.started)
        .unwrap();
    assert_eq!(folded.owner, attempt.owner);
    assert!(folded.boundary_settled.is_none());
    assert!(matches!(
        folded.finished.as_ref().map(|finish| &finish.settlement),
        Some(InstructionSettlement::Unknown {
            reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
            recovery: true,
            ..
        })
    ));
    assert_eq!(
        resumed
            .sup
            .world()
            .instructions
            .unresolved_launch_attempts(attempt.agent)
            .count(),
        1
    );
}
