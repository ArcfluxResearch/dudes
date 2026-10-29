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
    replace_after_hooks: Arc<Mutex<Option<PathBuf>>>,
}

impl CountingHost {
    fn new() -> Self {
        Self {
            inner: common::SharedHost::new(),
            calls: Arc::new(AtomicUsize::new(0)),
            replace_after_hooks: Arc::new(Mutex::new(None)),
        }
    }

    fn refusing() -> Self {
        Self {
            inner: common::SharedHost::with(dudes_supervisor::testing::FakeHost {
                fail_spawn: Some(io::ErrorKind::PermissionDenied),
                ..dudes_supervisor::testing::FakeHost::wall_clock_anchored()
            }),
            calls: Arc::new(AtomicUsize::new(0)),
            replace_after_hooks: Arc::new(Mutex::new(None)),
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
        self.inner.write_hooks(install)?;
        if let Some(root) = self.replace_after_hooks.lock().unwrap().take() {
            std::fs::rename(&root, root.with_extension("preserved"))?;
            std::fs::create_dir(&root)?;
            std::fs::create_dir(root.join(".git"))?;
        }
        Ok(())
    }
}

struct CountingPtyHost(Arc<AtomicUsize>);

impl crate::ports::PtyHostPort for CountingPtyHost {
    fn open(
        &mut self,
        spec: LaunchSpec,
        cols: u16,
        rows: u16,
    ) -> Result<crate::ports::OpenedPty, PtyOpenFailure> {
        self.0.fetch_add(1, Ordering::SeqCst);
        crate::ports::ClosedPtyHost.open(spec, cols, rows)
    }

    fn write(&mut self, pty: PtyId, data: &[u8]) -> io::Result<usize> {
        crate::ports::ClosedPtyHost.write(pty, data)
    }

    fn resize(&mut self, pty: PtyId, cols: u16, rows: u16) -> io::Result<()> {
        crate::ports::ClosedPtyHost.resize(pty, cols, rows)
    }

    fn subscribe(&mut self, pty: PtyId) -> Option<crate::pty_output::OutputSubscription> {
        crate::ports::ClosedPtyHost.subscribe(pty)
    }

    fn close(&mut self, pty: PtyId) -> PtyCloseOutcome {
        crate::ports::ClosedPtyHost.close(pty)
    }

    fn shutdown(&mut self) -> io::Result<()> {
        crate::ports::ClosedPtyHost.shutdown()
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

fn pump_queued(daemon: &mut TestDaemon, queue: &mut VecDeque<Input>) {
    while let Some(input) = queue.pop_front() {
        daemon.pump(input).unwrap();
    }
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
    prepared_with_sample(daemon, fixture, provider, |_| {})
}

fn prepared_with_sample(
    daemon: &mut TestDaemon,
    fixture: &common::Fixture,
    provider: Provider,
    mut sample: impl FnMut(&mut Input),
) -> Prepared {
    let mut hire = common::hire_params(
        daemon
            .ctx
            .enrollment
            .profiles
            .first()
            .map_or(fixture.state.path(), |profile| profile.cwd.as_path()),
    );
    hire.provider = provider;
    let Answered::Hired(hired) = daemon
        .ask(Ask::Hire(Box::new(hire)))
        .expect("hire before the real owned launch")
    else {
        panic!("expected a hired managed agent")
    };
    let agent = hired.agent;
    daemon
        .sup
        .observe_repository_admission(daemon.ctx.repository_observation(daemon.sup.world()));
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
        let mut input = next_input(daemon, &mut queue);
        sample(&mut input);
        daemon
            .sup
            .handle(input, &dudes_supervisor::RecoveryOperation::empty())
            .unwrap();
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

fn legacy_enrollment(root: PathBuf) -> crate::floor_actions::Enrollment {
    use crate::floor_actions::EnrolledCheck;
    crate::floor_actions::Enrollment {
        profiles: vec![Profile {
            id: "service".into(),
            label: "Service".into(),
            checks: vec![EnrolledCheck::enrolled(
                "Check",
                Predicate::Command {
                    argv: vec![common::true_program().into()],
                    timeout_s: 1,
                },
            )],
            cwd: root.clone(),
            written_cwd: root,
            repositories: None,
            department: "infra".into(),
            isolation: dudes_types::IsolationMode::Default,
            agent_budget: dudes_proto::AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            goal_budget: dudes_proto::GoalBudget {
                usd: None,
                tokens: None,
            },
            hop_cap: 3,
            mark: None,
        }],
    }
}

#[test]
fn an_attached_open_rechecks_a_known_origin_after_hook_preparation() {
    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
    let root = fixture.state.path().join("origin");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let root = root.canonicalize().unwrap();
    daemon.ctx.enrollment = legacy_enrollment(root.clone());
    daemon.observe_repository_enrollment().unwrap();
    let Answered::Hired(hired) = daemon
        .ask(Ask::Hire(Box::new(common::hire_params(&root))))
        .unwrap()
    else {
        panic!("the real hire must precede the parked attachment");
    };
    let agent = hired.agent;
    let original = daemon.sup.world().agents[&agent].repositories.clone();
    let entry = Entry {
        at: daemon.ctx.clock.now(),
        record: Record::Attached {
            agent,
            by: dudes_proto::Actor::System,
            hook_port: None,
            provenance: None,
        },
    };
    let committed = daemon.ctx.append(daemon.sup.world(), &entry).unwrap();
    let (main, checkpoint) = committed.into_parts();
    for (lsn, entry) in std::iter::once(main).chain(checkpoint) {
        daemon
            .sup
            .handle(
                Input::Replayed { lsn, entry },
                &dudes_supervisor::RecoveryOperation::empty(),
            )
            .unwrap();
    }
    daemon
        .sup
        .observe_repository_admission(daemon.ctx.repository_observation(daemon.sup.world()));
    assert!(daemon.sup.repository_admitted(agent));
    let effect = Effect::OpenPty {
        agent,
        spec: LaunchSpec {
            argv: vec![common::true_program().into()],
            env: Vec::new(),
            cwd: root.clone(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        },
        install: HookInstall::default(),
        cols: 80,
        rows: 24,
    };
    let calls = Arc::new(AtomicUsize::new(0));
    daemon.ctx.pty_host = crate::classify::PtyBarrier::new(
        Box::new(CountingPtyHost(Arc::clone(&calls))),
        daemon.ctx.resolver.clone(),
    );
    *host.replace_after_hooks.lock().unwrap() = Some(root.clone());
    let mut queue = VecDeque::new();
    let mut report = PumpReport::attach(agent);
    daemon
        .ctx
        .execute(
            &effect,
            &mut queue,
            &mut report,
            &mut daemon.sup,
            &mut daemon.closing,
        )
        .unwrap();
    assert!(root.with_extension("preserved").is_dir());
    assert!(!daemon.ctx.repository_allowed(daemon.sup.world(), agent));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the PTY host opened a replaced origin"
    );
    daemon
        .pump_inputs(&mut report, PumpOrigin::Ordinary, queue, &[], |_, _| {
            Ok(Handled::default())
        })
        .unwrap();
    assert!(!daemon.sup.world().agents[&agent].attached);
    assert_eq!(daemon.sup.world().agents[&agent].repositories, original);
    assert_eq!(host.spawn_calls(), 0);
    assert!(daemon.ctx.pending_attached_calls.is_empty());
    assert!(fixture.records().iter().any(|record| matches!(
        record,
        Record::InstructionObservationFinished {
            settlement: InstructionSettlement::Unknown {
                reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                ..
            },
            ..
        }
    )));
    assert!(!fixture.records().iter().any(|record| matches!(
        record,
        Record::PtySpawned { .. } | Record::TurnSpawned { .. }
    )));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one fixture proves expansion refusal and restoration of the original immutable authority"
)]
fn current_expansion_holds_an_owed_legacy_launch_before_host_and_resume_cannot_override_it() {
    use crate::floor_actions::ConfiguredRepository;
    use dudes_proto::repository::{RepoId, RepositoryEntry, RepositorySet};

    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
    let root = fixture.state.path().join("project");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let root = root.canonicalize().unwrap();
    let other = fixture.runtime.path().canonicalize().unwrap();
    std::fs::create_dir(other.join(".git")).unwrap();
    daemon.ctx.enrollment = legacy_enrollment(root.clone());
    daemon.observe_repository_enrollment().unwrap();
    daemon
        .pump_or_refuse(Input::Command(Command::RecordProjectRepositories {
            project: "service".into(),
            repositories: daemon.ctx.current_repositories.observed["service"]
                .repositories
                .clone(),
        }))
        .unwrap();
    daemon.observe_repository_enrollment().unwrap();
    let singleton_enrollment = daemon.ctx.enrollment.clone();
    let original_catalog = daemon.ctx.current_repositories.observed["service"].lsn;
    let attempt = prepared(&mut daemon, &fixture, Provider::Claude);
    assert!(
        daemon.sup.world().agents[&attempt.agent]
            .repositories
            .workspace()
            .is_some()
    );
    assert_eq!(host.spawn_calls(), 0);
    let original = daemon.sup.world().agents[&attempt.agent]
        .repositories
        .clone();
    let mut candidate = daemon.ctx.enrollment.clone();
    candidate.profiles[0].repositories = Some(
        RepositorySet::new(
            RepoId::parse("primary").unwrap(),
            vec![
                RepositoryEntry {
                    repo_id: RepoId::parse("primary").unwrap(),
                    value: ConfiguredRepository {
                        label: "Primary".into(),
                        path: root.clone(),
                        written_path: root,
                    },
                },
                RepositoryEntry {
                    repo_id: RepoId::parse("other").unwrap(),
                    value: ConfiguredRepository {
                        label: "Other".into(),
                        path: other.clone(),
                        written_path: other,
                    },
                },
            ],
        )
        .unwrap(),
    );
    let sha256 = "a".repeat(64);
    let read = crate::enrollment_reload::ReloadRead {
        seq: 1,
        sha256: sha256.clone(),
        path: fixture.runtime.path().join("operator.toml"),
        config: crate::config::OperatorConfig {
            enrollment: candidate,
            ..crate::config::OperatorConfig::default()
        },
        memory_config: daemon.ctx.memory_config,
        apply: Some(dudes_proto::rpc::floor::EnrollmentApply {
            sha256,
            added: vec![],
            removed: vec![],
            changed: vec!["service".into()],
        }),
        pid: None,
    };
    assert!(daemon.enrollment_reload(&read).unwrap().applied);
    daemon
        .pump_or_refuse(Input::Command(Command::RecordProjectRepositories {
            project: "service".into(),
            repositories: daemon.ctx.current_repositories.observed["service"]
                .repositories
                .clone(),
        }))
        .unwrap();
    let before = fixture.entries();
    let mut queue = VecDeque::new();
    execute(&mut daemon, &attempt.spawn, &mut queue);
    pump_queued(&mut daemon, &mut queue);
    assert_eq!(host.spawn_calls(), 0);
    assert_eq!(fixture.entries(), before);
    assert!(!daemon.sup.repository_admitted(attempt.agent));
    assert!(!daemon.sup.world().agents[&attempt.agent].paused);
    assert_eq!(
        daemon.sup.world().agents[&attempt.agent].repositories,
        original
    );
    assert!(daemon.ask(Ask::Resume(attempt.agent)).is_err());
    assert_eq!(fixture.entries(), before);
    assert!(!fixture.entries().iter().any(|(_, entry)| matches!(entry.record, Record::TurnSpawned { turn, .. } if turn == attempt.turn)));
    let sha256 = "b".repeat(64);
    let read = crate::enrollment_reload::ReloadRead {
        seq: 2,
        sha256: sha256.clone(),
        config: crate::config::OperatorConfig {
            enrollment: singleton_enrollment,
            ..crate::config::OperatorConfig::default()
        },
        apply: Some(dudes_proto::rpc::floor::EnrollmentApply {
            sha256,
            added: vec![],
            removed: vec![],
            changed: vec!["service".into()],
        }),
        ..read
    };
    assert!(daemon.enrollment_reload(&read).unwrap().applied);
    assert_eq!(
        daemon.ctx.current_repositories.observed["service"].lsn,
        original_catalog
    );
    await_late_dispatch(&mut daemon, &host);
    assert_eq!(
        daemon.sup.world().agents[&attempt.agent].repositories,
        original
    );
    assert_eq!(daemon.sup.world().turns[&attempt.turn].task, attempt.task);
    assert_eq!(
        host.spawn_calls(),
        1,
        "restored singleton authority retries automatically"
    );
    assert!(
        fixture
            .records()
            .iter()
            .all(|record| !matches!(record, Record::Resume { .. }))
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LateRepositoryControl {
    None,
    Pause,
    NewParticipant,
    DropKnownInput,
}

fn await_late_dispatch(daemon: &mut TestDaemon, host: &CountingHost) {
    poll_late_dispatch(daemon, host, None);
}

fn poll_late_dispatch(daemon: &mut TestDaemon, host: &CountingHost, drop_from: Option<Lsn>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while host.spawn_calls() == 0 && std::time::Instant::now() < deadline {
        if let Ok(Job::Input(mut input)) = daemon.jobs.recv_timeout(Duration::from_millis(20)) {
            if let (
                Some(started),
                Input::InstructionSampled {
                    snapshot,
                    comparison,
                    ..
                },
            ) = (drop_from, &mut *input)
            {
                let before = daemon
                    .sup
                    .world()
                    .instructions
                    .attempt(started)
                    .unwrap()
                    .snapshot
                    .as_ref()
                    .unwrap();
                let dudes_proto::instruction_state::InstructionSnapshotEvidence::Legacy(before) =
                    before
                else {
                    unreachable!()
                };
                for gap in &before.gaps {
                    if !snapshot.gaps.contains(gap) {
                        snapshot.gaps.push(gap.clone());
                    }
                }
                let known = snapshot
                    .inputs
                    .iter()
                    .position(|input| {
                        matches!(
                            input.fingerprint,
                            dudes_proto::InstructionFingerprint::File { .. }
                        )
                    })
                    .expect("known sampled file before coverage loss");
                snapshot.inputs.remove(known);
                let compared = crate::instruction_sample::compare(before, snapshot);
                assert!(
                    compared.changes.is_empty(),
                    "ordinary ending comparison hides this loss under unknown coverage"
                );
                assert!(
                    compared.gaps.iter().all(|gap| before.gaps.contains(gap))
                        && before.gaps.iter().all(|gap| compared.gaps.contains(gap)),
                    "the ordinary comparison has no new gap to expose the lost known input"
                );
                *comparison = Some(Box::new(compared));
            }
            daemon.pump(*input).unwrap();
        }
    }
}

fn retain_existing_unknown_input_gap(input: &mut Input) {
    let Input::InstructionSampled { snapshot, .. } = input else {
        return;
    };
    snapshot
        .gaps
        .push(dudes_proto::InstructionCoverageGap::metadata_limit());
    let known = snapshot
        .inputs
        .iter()
        .find(|input| {
            matches!(
                input.fingerprint,
                dudes_proto::InstructionFingerprint::File { .. }
            )
        })
        .unwrap();
    snapshot.gaps.push(dudes_proto::InstructionCoverageGap {
        reason: dudes_proto::InstructionGapKind::UninventoriedMember,
        context: known.context,
        agent: Some(known.source.agent),
        path: Some(known.path.clone()),
        origin: dudes_proto::InstructionGapOrigin::Observed,
    });
}

fn assert_unchanged_late_issue(
    daemon: &TestDaemon,
    fixture: &common::Fixture,
    attempt: &Prepared,
    original_issue: &(Lsn, Entry),
    repositories: Option<&dudes_proto::repository::TurnRepositories>,
) {
    assert_eq!(daemon.sup.world().turns[&attempt.turn].task, attempt.task);
    assert_eq!(
        daemon.sup.world().turns[&attempt.turn]
            .repositories
            .as_ref(),
        repositories
    );
    assert!(fixture.entries().contains(original_issue));
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter_map(Record::turn_issue)
            .filter(|issue| issue.agent == attempt.agent)
            .count(),
        1
    );
}

fn assert_original_dispatch(host: &CountingHost, attempt: &Prepared, fixture: &common::Fixture) {
    assert_eq!(
        host.spawn_calls(),
        1,
        "the same owed turn dispatches exactly once"
    );
    let Effect::Spawn { spec, .. } = &attempt.spawn else {
        unreachable!()
    };
    assert_eq!(host.inner.spawned()[0].argv, spec.argv);
    assert!(fixture.records().iter().all(|record| !matches!(
        record,
        Record::Pause { .. } | Record::Resume { .. } | Record::AgentHeld { .. }
    )));
}

fn late_repository_loss_recovers(before_preparation: bool, control: LateRepositoryControl) {
    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
    let root = fixture.state.path().join("project");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let root = root.canonicalize().unwrap();
    daemon.ctx.enrollment = legacy_enrollment(root.clone());
    daemon.observe_repository_enrollment().unwrap();
    if control == LateRepositoryControl::DropKnownInput {
        std::fs::write(root.join("AGENTS.md"), "Keep the known instruction file.\n").unwrap();
    }
    let attempt = if control == LateRepositoryControl::DropKnownInput {
        prepared_with_sample(
            &mut daemon,
            &fixture,
            Provider::Claude,
            retain_existing_unknown_input_gap,
        )
    } else {
        prepared(&mut daemon, &fixture, Provider::Claude)
    };
    let original_issue = fixture
        .entries()
        .into_iter()
        .find(|(lsn, _)| *lsn == attempt.issue)
        .unwrap();
    let repositories = daemon.sup.world().turns[&attempt.turn].repositories.clone();
    let preserved = root.with_file_name("preserved-project");
    if before_preparation {
        std::fs::rename(&root, &preserved).unwrap();
    } else {
        let root = root.clone();
        let preserved = preserved.clone();
        daemon.ctx.before_recovery_dispatch = Some(Box::new(move || {
            std::fs::rename(root, preserved).unwrap();
        }));
    }
    let mut queue = VecDeque::new();
    execute(&mut daemon, &attempt.spawn, &mut queue);
    pump_queued(&mut daemon, &mut queue);
    assert_eq!(host.spawn_calls(), 0);
    assert!(
        daemon.sup.world().agents[&attempt.agent]
            .automatic_hold
            .is_none(),
        "repository-only loss became an independent automatic hold"
    );
    assert!(daemon.sup.world().turns[&attempt.turn].is_issued_not_spawned());
    if control == LateRepositoryControl::Pause {
        daemon.ask(Ask::Pause(attempt.agent)).unwrap();
    }
    std::fs::rename(preserved, &root).unwrap();
    if control == LateRepositoryControl::NewParticipant {
        daemon
            .ask(Ask::Hire(Box::new(common::hire_params(&root))))
            .unwrap();
    }
    daemon.pump(Input::Tick).unwrap();
    poll_late_dispatch(
        &mut daemon,
        &host,
        (control == LateRepositoryControl::DropKnownInput).then_some(attempt.started),
    );
    assert_unchanged_late_issue(
        &daemon,
        &fixture,
        &attempt,
        &original_issue,
        repositories.as_ref(),
    );
    if control != LateRepositoryControl::None {
        assert_eq!(
            host.spawn_calls(),
            0,
            "a standing control or participant drift dispatched"
        );
        assert!(daemon.sup.world().turns[&attempt.turn].is_issued_not_spawned());
        assert!(
            daemon.sup.world().agents[&attempt.agent]
                .automatic_hold
                .is_none()
        );
        assert!(
            fixture
                .records()
                .iter()
                .all(|record| !matches!(record, Record::Resume { .. }))
        );
        return;
    }
    assert_original_dispatch(&host, &attempt, &fixture);
    daemon.pump(Input::Tick).unwrap();
    assert_original_dispatch(&host, &attempt, &fixture);
}

#[test]
fn repository_loss_before_preparation_recovers_the_same_turn_without_a_launch_failure_hold() {
    late_repository_loss_recovers(true, LateRepositoryControl::None);
}

#[test]
fn repository_loss_at_the_final_host_cut_releases_only_the_no_host_claim_and_recovers() {
    late_repository_loss_recovers(false, LateRepositoryControl::None);
}

#[test]
fn a_no_host_repository_continuation_keeps_an_operator_pause() {
    late_repository_loss_recovers(false, LateRepositoryControl::Pause);
}

#[test]
fn a_no_host_repository_continuation_refuses_new_participant_coverage() {
    late_repository_loss_recovers(false, LateRepositoryControl::NewParticipant);
}

#[test]
fn a_no_host_continuation_cannot_lose_a_known_file_under_an_unchanged_unknown_gap() {
    late_repository_loss_recovers(false, LateRepositoryControl::DropKnownInput);
}

#[test]
fn a_revalidation_response_waits_for_the_pending_operator_pause_acknowledgement() {
    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
    let root = fixture.state.path().join("project");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let root = root.canonicalize().unwrap();
    daemon.ctx.enrollment = legacy_enrollment(root.clone());
    daemon.observe_repository_enrollment().unwrap();
    let attempt = prepared(&mut daemon, &fixture, Provider::Claude);
    let preserved = root.with_file_name("preserved-project");
    std::fs::rename(&root, &preserved).unwrap();
    let mut queue = VecDeque::new();
    execute(&mut daemon, &attempt.spawn, &mut queue);
    pump_queued(&mut daemon, &mut queue);
    std::fs::rename(preserved, root).unwrap();
    daemon.pump(Input::Tick).unwrap();
    let response = next_input(&daemon, &mut queue);
    let Input::InstructionSampled {
        request: old_request,
        ..
    } = &response
    else {
        panic!("fresh revalidation response");
    };
    let old_request = *old_request;
    daemon
        .sup
        .handle(
            Input::Command(Command::Pause {
                agent: attempt.agent,
                browser: None,
            }),
            &dudes_supervisor::RecoveryOperation::empty(),
        )
        .unwrap();
    let pause = daemon.sup.take_effects();
    assert!(
        matches!(&pause[..], [Effect::Persist { entry, .. }] if matches!(entry.record, Record::Pause { .. }))
    );
    execute(&mut daemon, &pause[0], &mut queue);
    assert!(
        !daemon.sup.world().agents[&attempt.agent].paused,
        "the acknowledgement is held"
    );
    daemon.pump(response).unwrap();
    assert_eq!(
        host.spawn_calls(),
        0,
        "revalidation outran a pending operator pause"
    );
    pump_queued(&mut daemon, &mut queue);
    assert!(daemon.sup.world().agents[&attempt.agent].paused);
    daemon.pump(Input::Tick).unwrap();
    assert_eq!(host.spawn_calls(), 0);
    daemon.ask(Ask::Resume(attempt.agent)).unwrap();
    let fresh = next_input(&daemon, &mut queue);
    let Input::InstructionSampled { request, .. } = &fresh else {
        panic!("resumed revalidation response");
    };
    assert_ne!(
        *request, old_request,
        "the pre-control proof cannot be reused"
    );
    daemon.pump(fresh).unwrap();
    assert_eq!(host.spawn_calls(), 1);
    assert_eq!(daemon.sup.world().turns[&attempt.turn].task, attempt.task);
}

#[test]
fn a_no_host_finite_dispatch_reuses_its_current_intent_without_charging_uncertainty() {
    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
    let root = fixture.state.path().join("project-b");
    let a_root = fixture.state.path().join("project-a");
    for root in [&root, &a_root] {
        std::fs::create_dir_all(root.join(".git")).unwrap();
    }
    let root = root.canonicalize().unwrap();
    let a_root = a_root.canonicalize().unwrap();
    daemon.ctx.enrollment = legacy_enrollment(root.clone());
    daemon.observe_repository_enrollment().unwrap();
    let Answered::Hired(a) = daemon
        .ask(Ask::Hire(Box::new(common::hire_params(&a_root))))
        .unwrap()
    else {
        unreachable!()
    };
    let Answered::Hired(b) = daemon
        .ask(Ask::Hire(Box::new(common::hire_params(&root))))
        .unwrap()
    else {
        unreachable!()
    };
    let agent = b.agent;
    daemon.pump(Input::Tick).unwrap();
    let sessions = crate::floor_session::FloorSessions::new(
        "127.0.0.1:31337".parse().unwrap(),
        Arc::clone(&daemon.ctx.clock),
    );
    let session = sessions
        .admitted_for_test("127.0.0.1:31337", "http://127.0.0.1:31337")
        .0;
    daemon.ctx.browser_session = Some(session.clone());
    let preserved = root.with_file_name("preserved-project-b");
    let hidden = root.clone();
    let saved = preserved.clone();
    daemon.ctx.before_recovery_dispatch = Some(Box::new(move || {
        std::fs::rename(hidden, saved).unwrap();
    }));
    let context_lsn = daemon.sup.world().agents[&agent].context_lsn;
    daemon
        .pump_runtime_input(Input::Command(Command::ConversationMessageQueued {
            agent,
            text: "Keep this finite task in project B".into(),
            context_lsn,
            context_reset_turn: None,
            browser: dudes_proto::BrowserNoteAttribution {
                request_id_digest: dudes_proto::record::Hex32::new([3; 32]),
                request_digest: dudes_proto::record::Hex32::new([4; 32]),
                session_digest: session.digest(),
            },
        }))
        .unwrap();
    let mut queue = VecDeque::new();
    for _ in 0..16 {
        if !root.exists() {
            break;
        }
        let input = next_input(&daemon, &mut queue);
        daemon.pump_runtime_input(input).unwrap();
    }
    assert!(!root.exists());
    assert_eq!(host.spawn_calls(), 0);
    let turn = daemon.sup.world().agents[&agent].open_turn.unwrap();
    let original = fixture
        .entries()
        .into_iter()
        .find(|(_, entry)| {
            entry
                .record
                .turn_issue()
                .is_some_and(|issue| issue.turn == turn)
        })
        .unwrap();
    let intents = daemon
        .sup
        .world()
        .browser_issue(turn)
        .unwrap()
        .dispatch_intents
        .clone();
    assert_eq!(intents.len(), 1);
    let day = daemon.ctx.clock.now().utc_day();
    let goal = GoalId::from_parts(0, 0);
    let before = daemon
        .ctx
        .projections
        .spend_views(agent, goal, day)
        .unwrap();
    let per_turn = daemon.sup.config().per_turn_floor;
    let reserve = (
        dudes_policy::budget::reserved(
            before.agent_today,
            before.agent_unknown_ends_today,
            per_turn,
        ),
        dudes_policy::budget::reserved(
            before.floor_today,
            before.floor_unknown_ends_today,
            per_turn,
        ),
    );
    std::fs::rename(preserved, root).unwrap();
    daemon.pump(Input::Tick).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while host.spawn_calls() == 0 && std::time::Instant::now() < deadline {
        if let Ok(Job::Input(input)) = daemon.jobs.recv_timeout(Duration::from_millis(20)) {
            daemon.pump_runtime_input(*input).unwrap();
        }
    }
    let after = daemon
        .ctx
        .projections
        .spend_views(agent, goal, day)
        .unwrap();
    assert_eq!(
        after, before,
        "a known no-host dispatch created uncertainty charges"
    );
    assert_eq!(
        (
            dudes_policy::budget::reserved(
                after.agent_today,
                after.agent_unknown_ends_today,
                per_turn
            ),
            dudes_policy::budget::reserved(
                after.floor_today,
                after.floor_unknown_ends_today,
                per_turn
            ),
        ),
        reserve
    );
    assert_eq!(
        daemon
            .sup
            .world()
            .browser_issue(turn)
            .unwrap()
            .dispatch_intents,
        intents
    );
    assert!(fixture.entries().contains(&original));
    assert_eq!(host.spawn_calls(), 1);
    assert!(daemon.sup.repository_admitted(a.agent));
}

#[test]
fn unresolved_current_admission_refuses_a_parked_finite_issue_with_no_charged_goal() {
    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
    let root = fixture.state.path().canonicalize().unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
    daemon.ctx.enrollment = legacy_enrollment(root.clone());
    daemon.observe_repository_enrollment().unwrap();
    let Answered::Hired(hired) = daemon
        .ask(Ask::Hire(Box::new(common::hire_params(&root))))
        .unwrap()
    else {
        panic!("the singleton hire is admitted");
    };
    let agent = hired.agent;
    daemon.pump(Input::Tick).unwrap();
    daemon
        .sup
        .observe_repository_admission(daemon.ctx.repository_observation(daemon.sup.world()));
    let context_lsn = daemon.sup.world().agents[&agent].context_lsn;
    let sessions = crate::floor_session::FloorSessions::new(
        "127.0.0.1:31337".parse().unwrap(),
        Arc::clone(&daemon.ctx.clock),
    );
    let session = sessions
        .admitted_for_test("127.0.0.1:31337", "http://127.0.0.1:31337")
        .0;
    daemon.ctx.browser_session = Some(session.clone());
    let mut queue = VecDeque::from([Input::Command(Command::ConversationMessageQueued {
        agent,
        text: "Check the finite conversation".into(),
        context_lsn,
        context_reset_turn: None,
        browser: dudes_proto::BrowserNoteAttribution {
            request_id_digest: dudes_proto::record::Hex32::new([1; 32]),
            request_digest: dudes_proto::record::Hex32::new([2; 32]),
            session_digest: session.digest(),
        },
    })]);
    for _ in 0..64 {
        let input = next_input(&daemon, &mut queue);
        daemon
            .sup
            .handle(input, &dudes_supervisor::RecoveryOperation::empty())
            .unwrap();
        for effect in daemon.sup.take_effects() {
            if let Effect::Persist { entry, .. } = &effect
                && let Some(issue) = entry.record.turn_issue()
            {
                assert_eq!(issue.goal, None);
                assert_eq!(daemon.sup.world().agents[&agent].goal, None);
                daemon.ctx.enrollment.profiles.clear();
                let before = fixture.entries();
                assert!(
                    daemon
                        .ctx
                        .execute(
                            &effect,
                            &mut queue,
                            &mut PumpReport::Discard,
                            &mut daemon.sup,
                            &mut daemon.closing
                        )
                        .is_err()
                );
                assert_eq!(fixture.entries(), before);
                assert_eq!(host.spawn_calls(), 0);
                assert!(
                    fixture
                        .entries()
                        .iter()
                        .all(|(_, entry)| entry.record.turn_issue().is_none())
                );
                assert!(!daemon.ctx.repository_allowed(daemon.sup.world(), agent));
                assert!(!daemon.sup.world().agents[&agent].paused);
                return;
            }
            execute(&mut daemon, &effect, &mut queue);
        }
    }
    panic!("finite work did not reach its parked issue boundary");
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
        daemon
            .sup
            .handle(input, &dudes_supervisor::RecoveryOperation::empty())
            .unwrap();
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
            Input::Persisted { token, append } if *token == end_token && append.lsn() == end_lsn)
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
        daemon
            .sup
            .handle(input, &dudes_supervisor::RecoveryOperation::empty())
            .unwrap();
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
    daemon
        .sup
        .handle(ack, &dudes_supervisor::RecoveryOperation::empty())
        .unwrap();
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
    daemon
        .sup
        .handle(ack, &dudes_supervisor::RecoveryOperation::empty())
        .unwrap();
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
        Input::InstructionJournaled { append }
            if matches!(append.entry().record,
                Record::InstructionAttemptBoundarySettled {
                    owner,
                    started_lsn,
                    proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                        receipt: InstructionNativeBoundaryReceipt::LaunchFailureEnded { lsn },
                    },
                } if owner == attempt.owner && started_lsn == attempt.started && lsn == end_lsn)));
    daemon
        .sup
        .handle(native, &dudes_supervisor::RecoveryOperation::empty())
        .unwrap();
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
        .expect("real full-fsync hold append before crash")
        .lsn();
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
