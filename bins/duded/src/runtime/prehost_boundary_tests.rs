mod recovery_census;

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
    installs: Arc<Mutex<Vec<ManagedInstall>>>,
}

impl CountingHost {
    fn new() -> Self {
        Self {
            inner: common::SharedHost::new(),
            calls: Arc::new(AtomicUsize::new(0)),
            replace_after_hooks: Arc::new(Mutex::new(None)),
            installs: Arc::default(),
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
            installs: Arc::default(),
        }
    }

    fn spawn_calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ProcessHost for CountingHost {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.installs.lock().unwrap().push(install.clone());
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

    fn owned_targets(&self, limit: usize) -> dudes_supervisor::host::OwnedTargets {
        self.inner.owned_targets(limit)
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
    boot_settings(&settings, host, provider, command)
}

fn boot_settings(
    settings: &crate::Settings,
    host: &CountingHost,
    provider: Provider,
    command: &str,
) -> TestDaemon {
    let mut seams = common::seams(
        settings,
        host.clone(),
        Arc::new(common::ScriptedRunner::default()),
        Arc::new(common::TestClock::new()),
        Witness::default(),
    );
    let adapter: Box<dyn dudes_providers::adapter::Adapter> = match provider {
        Provider::Claude => Box::new(dudes_providers::claude::ClaudeAdapter::new(
            dudes_providers::claude::config::ClaudeConfig {
                command: command.into(),
                permission_mode: dudes_providers::claude::config::PermissionMode::Default,
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
    crate::boot(settings, seams).expect("real pre-host fixture boot")
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
    prepared_for_agent(daemon, fixture, agent)
}

fn prepared_for_agent(
    daemon: &mut TestDaemon,
    fixture: &common::Fixture,
    agent: AgentId,
) -> Prepared {
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
        let input = next_input(daemon, &mut queue);
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

struct ScopedFixture {
    setup: super::repository_hire_tests::Setup,
    host: CountingHost,
    daemon: TestDaemon,
    issued: Prepared,
}

struct LiveScopedFixture {
    setup: super::repository_hire_tests::Setup,
    host: CountingHost,
    daemon: TestDaemon,
    agent: AgentId,
    turn: TurnId,
    note_lsn: Lsn,
    task: String,
    session: crate::floor_session::SessionIdentity,
    pause: crate::BrowserLaunchPause,
}

impl LiveScopedFixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let setup = super::repository_hire_tests::Setup::with_repositories(&["api"]);
        let (mut running, hires, params) = setup.start(true);
        let agent = hires.hire(params).unwrap().agent;
        running.stop(ExitCause::Ended);
        let host = CountingHost::new();
        let program = setup.fixture.runtime.path().join("claude-live-fixture");
        std::fs::write(&program, b"owned Claude fixture executable").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let program = program.canonicalize().unwrap();
        let mut daemon = boot_settings(
            &setup.settings,
            &host,
            Provider::Claude,
            program.to_str().unwrap(),
        );
        daemon.ctx.current_repositories.test_hire_capability = true;
        daemon
            .sup
            .observe_repository_admission(daemon.ctx.repository_observation(daemon.sup.world()));
        daemon
            .ctx
            .children
            .private_roots()
            .qualify_claude_fixture(b"owned Claude fixture executable");
        daemon
            .pump(Input::Command(Command::Resume {
                agent,
                browser: None,
            }))
            .unwrap();
        append_scoped_fixture(
            &mut daemon,
            Record::GoalSet {
                agent,
                goal: dudes_proto::Goal {
                    id: GoalId::from_parts(6, 1),
                    text: "Finish the scoped task".into(),
                    predicate: dudes_proto::Predicate::Command {
                        argv: vec![common::true_program().into()],
                        timeout_s: 1,
                    },
                    budget: dudes_proto::GoalBudget {
                        usd: None,
                        tokens: None,
                    },
                    hop_cap: 4,
                    completion_check: None,
                    baseline: Some(dudes_proto::CheckBaseline::MayAlreadyPass),
                },
                browser_onboarding: None,
                browser_goal: None,
                goal_replacement: None,
            },
        );
        let sessions = crate::floor_session::FloorSessions::new(
            "127.0.0.1:31337".parse().unwrap(),
            Arc::new(common::TestClock::new()),
        );
        let session = sessions
            .admitted_for_test("127.0.0.1:31337", "http://127.0.0.1:31337")
            .0;
        let pause = daemon.browser_launch_pause();
        pause.arm();
        let request = Self::request(
            crate::floor_chat::ChatVerb::Send,
            agent,
            "text",
            "Retain this exact goal note",
            'B',
        );
        let crate::floor_chat::ChatOutcome::Queued { note_lsn, .. } =
            daemon.browser_chat(&session, &request).unwrap()
        else {
            panic!("durable goal note")
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !pause.wait(Duration::from_millis(10)) {
            assert!(
                std::time::Instant::now() < deadline,
                "scoped worker finishes"
            );
            match daemon.jobs.recv_timeout(Duration::from_millis(10)) {
                Ok(Job::Input(input)) => daemon.pump_runtime_input(*input).unwrap(),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                other => panic!("expected scoped preparation input: {}", other.is_ok()),
            }
        }
        let turn = daemon.sup.world().agents[&agent].open_turn.unwrap();
        Self {
            setup,
            host,
            task: daemon.sup.world().turns[&turn].task.clone(),
            daemon,
            agent,
            turn,
            note_lsn,
            session,
            pause,
        }
    }

    fn request(
        verb: crate::floor_chat::ChatVerb,
        agent: AgentId,
        field: &str,
        value: &str,
        key: char,
    ) -> crate::floor_chat::ChatRequest {
        let mut request_id = "A".repeat(43);
        request_id.replace_range(..1, &key.to_string());
        let body =
            serde_json::json!({"request_id":request_id,"agent":agent.to_string(),field:value});
        crate::floor_chat::ChatRequest::parse(verb, &serde_json::to_vec(&body).unwrap()).unwrap()
    }

    fn completion(&mut self) -> crate::chat_launch::ScopedCompletion {
        assert_eq!(self.host.spawn_calls(), 0);
        self.pause.release();
        let Job::ScopedLaunchPrepared(completion) = self
            .daemon
            .jobs
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
        else {
            panic!("exact scoped completion")
        };
        *completion
    }
}

impl Drop for LiveScopedFixture {
    fn drop(&mut self) {
        self.pause.release();
    }
}

impl ScopedFixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let setup = super::repository_hire_tests::Setup::with_repositories(&["api"]);
        let (mut running, hires, params) = setup.start(true);
        let agent = hires.hire(params).unwrap().agent;
        running.stop(ExitCause::Ended);
        let host = CountingHost::new();
        let program = setup.fixture.runtime.path().join("claude-fixture");
        std::fs::write(&program, b"owned Claude fixture executable").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let program = program.canonicalize().unwrap();
        let mut daemon = boot_settings(
            &setup.settings,
            &host,
            Provider::Claude,
            program.to_str().unwrap(),
        );
        daemon.ctx.current_repositories.test_hire_capability = true;
        daemon
            .sup
            .observe_repository_admission(daemon.ctx.repository_observation(daemon.sup.world()));
        daemon
            .ctx
            .children
            .private_roots()
            .qualify_claude_fixture(b"owned Claude fixture executable");
        let issued = scoped_issued_fixture(&mut daemon, agent);
        Self {
            setup,
            host,
            daemon,
            issued,
        }
    }

    fn prepare(&mut self) -> crate::chat_launch::ScopedCompletion {
        assert!(
            self.daemon
                .ctx
                .recovery_dispatch_allowed(&self.daemon.sup, self.issued.turn)
        );
        let mut queue = VecDeque::new();
        let Effect::Spawn { launch, .. } = &self.issued.spawn else {
            unreachable!();
        };
        self.daemon.ctx.prepare_scoped_turn(
            self.issued.turn,
            None,
            launch,
            None,
            &mut queue,
            &self.daemon.sup,
        );
        assert!(queue.is_empty(), "scoped work runs in the existing worker");
        assert_eq!(self.host.spawn_calls(), 0);
        let Job::ScopedLaunchPrepared(completion) = self
            .daemon
            .jobs
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
        else {
            panic!("private scoped worker completion");
        };
        assert!(
            completion.prepared.is_ok(),
            "{:?}",
            completion.prepared.as_ref().err()
        );
        *completion
    }
}

fn append_scoped_fixture(daemon: &mut TestDaemon, record: Record) -> Lsn {
    let entry = Entry {
        at: daemon.ctx.clock.now(),
        record,
    };
    let (lsn, crc) = daemon.ctx.writer.append_witnessed(&entry).unwrap();
    daemon.ctx.projections.apply(lsn, crc, &entry).unwrap();
    daemon
        .sup
        .handle(
            Input::Replayed { lsn, entry },
            &dudes_supervisor::RecoveryOperation::empty(),
        )
        .unwrap();
    assert!(daemon.sup.take_effects().is_empty());
    lsn
}

fn scoped_issued_fixture(daemon: &mut TestDaemon, agent: AgentId) -> Prepared {
    use dudes_proto::repository::{HireRepositories, RepositoryTurnGrant, RepositoryTurnIssued};
    use dudes_proto::repository_instruction::{
        RepositoryInstructionBinding, RepositoryInstructionSnapshot, RepositoryInstructionStart,
    };
    let turn = TurnId::from_parts(5, 1);
    let task = "exact immutable scoped task";
    let owner = InstructionObservationOwner::Managed {
        agent,
        turn,
        issue: InstructionIssueAssociation::Prospective,
    };
    let row = &daemon.sup.world().agents[&agent];
    let HireRepositories::Scoped(hire) = &row.repositories else {
        panic!("scoped hire fixture");
    };
    let grant = RepositoryTurnGrant::select(row.mcp.hire_lsn, hire, None).unwrap();
    let Record::InstructionObservationStarted { snapshot, .. } =
        dudes_proto::record::samples::record(
            dudes_proto::RecordKind::InstructionObservationStarted,
        )
    else {
        unreachable!();
    };
    let batch = RepositoryInstructionStart {
        v: dudes_proto::repository_instruction::INSTRUCTION_BATCH_VERSION,
        binding: RepositoryInstructionBinding {
            hired_lsn: grant.hired_lsn,
            ticket_start_lsn: None,
        },
        owner,
        ticket: None,
        members: grant
            .repositories
            .entries()
            .iter()
            .map(|entry| {
                let mut snapshot = snapshot.clone();
                snapshot.execution_folder = Some(
                    dudes_proto::instruction::NativeInstructionPath::from_path(
                        &entry.value.execution.directory,
                    )
                    .unwrap(),
                );
                snapshot.gaps[0].agent = Some(agent);
                RepositoryInstructionSnapshot {
                    repo_id: entry.repo_id.clone(),
                    root: Some(entry.value.execution.directory_identity),
                    snapshot,
                }
            })
            .collect(),
    };
    let started = append_scoped_fixture(daemon, Record::InstructionObservationBatchStarted(batch));
    let (session_id, selection, payload, note_lsn) =
        scoped_fixture_payload(daemon, agent, turn, task);
    let issue = append_scoped_fixture(
        daemon,
        Record::TurnIssuedWithRepositories(Box::new(RepositoryTurnIssued {
            grant: grant.clone(),
            payload,
        })),
    );
    append_scoped_fixture(
        daemon,
        Record::BrowserNoteDispatchIntent {
            agent,
            note_lsn,
            turn,
        },
    );
    Prepared {
        agent,
        turn,
        started,
        owner,
        ticket: None,
        issue,
        task: task.into(),
        spawn: Effect::Spawn {
            turn,
            launch: dudes_supervisor::ManagedSpawn::Scoped {
                task: task.into(),
                session: dudes_providers::adapter::TurnSession::First(session_id),
                selection,
                env: Vec::new(),
            },
            mcp_grant: None,
        },
    }
}

fn scoped_fixture_payload(
    daemon: &mut TestDaemon,
    agent: AgentId,
    turn: TurnId,
    task: &str,
) -> (
    dudes_types::SessionId,
    dudes_types::ModelSelection,
    dudes_proto::issue::ManagedIssuePayload,
    Lsn,
) {
    let goal = GoalId::from_parts(5, 2);
    append_scoped_fixture(
        daemon,
        Record::GoalSet {
            agent,
            goal: dudes_proto::Goal {
                id: goal,
                text: "Scoped fixture".into(),
                predicate: Predicate::Command {
                    argv: vec![common::true_program().into()],
                    timeout_s: 1,
                },
                completion_check: None,
                baseline: None,
                budget: dudes_proto::GoalBudget {
                    usd: None,
                    tokens: None,
                },
                hop_cap: 3,
            },
            browser_onboarding: None,
            browser_goal: None,
            goal_replacement: None,
        },
    );
    let context_lsn = append_scoped_fixture(daemon, Record::GoalCleared { agent, goal });
    let Record::BrowserNoteQueued { browser, .. } =
        dudes_proto::record::samples::record(dudes_proto::RecordKind::BrowserNoteQueued)
    else {
        unreachable!();
    };
    let charge_scope = dudes_proto::record::ConversationChargeScope::AgentAndFloor;
    let note_lsn = append_scoped_fixture(
        daemon,
        Record::ConversationMessageQueued {
            agent,
            text: task.into(),
            browser,
            context_lsn,
            charge_scope,
        },
    );
    let decision = append_scoped_fixture(
        daemon,
        Record::ConversationTurnDecided {
            agent,
            note_lsn,
            context_lsn,
            charge_scope,
            input_digest: "scoped fixture".into(),
            decision: dudes_proto::NoteAdmission::Issue { task: task.into() },
            gate_trace: Vec::new(),
        },
    );
    let session_id = dudes_types::SessionId::new("00000000-0000-7000-8000-000000000001");
    let selection = dudes_types::ModelSelection::named("claude-sonnet-4-6");
    let payload = Record::ConversationTurnIssued {
        agent,
        turn,
        note_lsn,
        context_lsn,
        charge_scope,
        task: task.into(),
        session: Some(session_id.clone()),
        model_selection: selection.clone(),
        lsn_of_decision: decision,
    }
    .issue_payload()
    .unwrap()
    .to_owned();
    (session_id, selection, payload, note_lsn)
}

#[test]
fn a_scoped_goal_note_stays_cancellable_until_actual_host_entry() {
    let mut f = LiveScopedFixture::new();
    assert!(
        matches!(f.daemon.sup.browser_note_cancel_disposition(f.agent, f.note_lsn), dudes_supervisor::machine::BrowserNoteCancelDisposition::CleanIssue { turn } if turn == f.turn)
    );
    let request = LiveScopedFixture::request(
        crate::floor_chat::ChatVerb::Cancel,
        f.agent,
        "note_lsn",
        &f.note_lsn.0.to_string(),
        'C',
    );
    let outcome = f.daemon.browser_chat(&f.session, &request);
    assert!(
        outcome.is_ok(),
        "{outcome:?}; records={:?}",
        f.setup
            .fixture
            .records()
            .iter()
            .map(Record::kind)
            .collect::<Vec<_>>()
    );
    let outcome = outcome.unwrap();
    assert!(!matches!(
        outcome,
        crate::floor_chat::ChatOutcome::Refused { .. }
    ));
    let completion = f.completion();
    f.daemon.complete_scoped_launch(completion).unwrap();
    assert_eq!(f.host.spawn_calls(), 0);
    assert!(f.daemon.ctx.scoped_launches.is_empty());
    assert!(f.daemon.sup.world().browser_issue_cancelled(f.turn));
    assert!(!f.setup.fixture.records().iter().any(|record| matches!(
        record,
        Record::TurnSpawned { .. } | Record::ConversationDispatchUncertain { .. }
    )));
}

fn control_during_scoped_preparation(halt: bool) {
    let mut f = LiveScopedFixture::new();
    let command = if halt {
        Command::Halt {
            agent: f.agent,
            text: None,
        }
    } else {
        Command::Pause {
            agent: f.agent,
            browser: None,
        }
    };
    f.daemon.pump(Input::Command(command)).unwrap();
    let completion = f.completion();
    f.daemon.complete_scoped_launch(completion).unwrap();
    assert_eq!(f.host.spawn_calls(), 0);
    assert!(
        f.daemon.sup.world().turns[&f.turn].has_ended(),
        "a controlled current preparation must settle as known not launched"
    );
    assert!(f.daemon.ctx.scoped_launches.is_empty());
    assert!(!f.daemon.sup.has_ungranted_browser_launch(f.agent));
    let before = f.setup.fixture.records();
    assert!(before.iter().any(|record| matches!(record, Record::TurnEnded { turn, outcome: Some(TurnOutcome::NotLaunched { .. }), .. } if *turn == f.turn)));
    assert!(before.iter().any(|record| matches!(record,
        Record::InstructionObservationBatchFinished(batch)
            if matches!(batch.owner, InstructionObservationOwner::Managed { turn, .. } if turn == f.turn)
                && batch.members.iter().all(|member| matches!(member.settlement,
                    InstructionSettlement::NoProcess { proof: InstructionNoProcessProof::NoHostCall, .. })))));
    assert!(
        !before
            .iter()
            .any(|record| matches!(record, Record::ConversationDispatchUncertain { .. }))
    );
    f.daemon
        .pump(Input::Command(Command::Resume {
            agent: f.agent,
            browser: None,
        }))
        .unwrap();
    let after = f.setup.fixture.records();
    assert_eq!(
        after
            .iter()
            .filter(|record| record
                .turn_issue()
                .is_some_and(|issue| issue.turn == f.turn))
            .count(),
        1
    );
    assert_eq!(
        after
            .iter()
            .filter(|record| matches!(record, Record::TurnEnded { turn, .. } if *turn == f.turn))
            .count(),
        1
    );
    assert!(
        !after
            .iter()
            .any(|record| matches!(record, Record::TurnSpawned { turn, .. } if *turn == f.turn))
    );
}

#[test]
fn pause_during_scoped_preparation_settles_the_current_known_no_host_attempt() {
    control_during_scoped_preparation(false);
}

#[test]
fn halt_during_scoped_preparation_settles_the_current_known_no_host_attempt() {
    control_during_scoped_preparation(true);
}

#[test]
fn a_fresh_scoped_conversation_preparation_failure_has_no_false_dispatch_uncertainty() {
    let setup = super::repository_hire_tests::Setup::with_repositories(&["api"]);
    let (mut running, hires, params) = setup.start(true);
    let agent = hires.hire(params).unwrap().agent;
    running.stop(ExitCause::Ended);
    let host = CountingHost::new();
    let mut daemon = boot_settings(
        &setup.settings,
        &host,
        Provider::Claude,
        common::true_program(),
    );
    daemon.ctx.current_repositories.test_hire_capability = true;
    daemon
        .sup
        .observe_repository_admission(daemon.ctx.repository_observation(daemon.sup.world()));
    let context_lsn = daemon.sup.world().agents[&agent].context_lsn;
    append_scoped_fixture(
        &mut daemon,
        Record::ConversationMessageQueued {
            agent,
            text: "Read the declared task".into(),
            browser: dudes_proto::BrowserNoteAttribution {
                request_id_digest: dudes_proto::record::Hex32::new([91; 32]),
                request_digest: dudes_proto::record::Hex32::new([92; 32]),
                session_digest: dudes_proto::record::Hex32::new([93; 32]),
            },
            context_lsn,
            charge_scope: dudes_proto::ConversationChargeScope::AgentAndFloor,
        },
    );
    daemon
        .pump(Input::Command(Command::Resume {
            agent,
            browser: None,
        }))
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if daemon
            .sup
            .world()
            .turns
            .values()
            .any(dudes_supervisor::world::TurnRow::has_ended)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the bounded scoped preparation settles; records={:?}, paused={}, hold={:?}, turns={:?}",
            setup
                .fixture
                .records()
                .iter()
                .map(Record::kind)
                .collect::<Vec<_>>(),
            daemon.sup.world().agents[&agent].paused,
            daemon.sup.world().agents[&agent].automatic_hold,
            daemon
                .sup
                .world()
                .turns
                .iter()
                .map(|(turn, row)| (*turn, row.status))
                .collect::<Vec<_>>()
        );
        match daemon.jobs.recv_timeout(Duration::from_millis(100)) {
            Ok(Job::Input(input)) => daemon.pump_runtime_input(*input).unwrap(),
            Ok(Job::ScopedLaunchPrepared(completion)) => {
                daemon.complete_scoped_launch(*completion).unwrap();
            }
            Ok(_) => panic!("unexpected scoped preparation job"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("scoped preparation stopped: {error}"),
        }
    }
    let records = setup.fixture.records();
    assert!(records.iter().any(|record| matches!(record, Record::TurnIssuedWithRepositories(issue) if issue.payload.as_ref().kind() == dudes_proto::issue::ManagedIssueKind::IdentifiedConversation)));
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::BrowserNoteDispatchIntent { .. }))
            .count(),
        0
    );
    assert!(records.iter().any(|record| matches!(
        record,
        Record::TurnEnded {
            outcome: Some(TurnOutcome::NotLaunched { .. }),
            ..
        }
    )));
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::ConversationDispatchUncertain { .. }))
    );
    assert_eq!(host.spawn_calls(), 0);
}

#[test]
fn scoped_preparation_refuses_task_model_or_session_drift_before_a_worker_or_host() {
    let mut f = ScopedFixture::new();
    let Effect::Spawn { launch, .. } = &f.issued.spawn else {
        unreachable!()
    };
    for changed in 0..3 {
        let mut seed = launch.clone();
        let dudes_supervisor::ManagedSpawn::Scoped {
            task,
            session,
            selection,
            ..
        } = &mut seed
        else {
            unreachable!()
        };
        match changed {
            0 => task.push_str(" changed"),
            1 => *selection = dudes_types::ModelSelection::named("unrecorded-model"),
            _ => {
                *session = dudes_providers::adapter::TurnSession::First(
                    dudes_types::SessionId::new("unrecorded-session"),
                );
            }
        }
        let mut queue = VecDeque::new();
        f.daemon.ctx.prepare_scoped_turn(
            f.issued.turn,
            None,
            &seed,
            None,
            &mut queue,
            &f.daemon.sup,
        );
        assert!(
            matches!(queue.pop_front(), Some(Input::LaunchPreparationFailed { turn, .. }) if turn == f.issued.turn)
        );
        assert!(f.daemon.jobs.try_recv().is_err());
        assert_eq!(f.host.spawn_calls(), 0);
        assert!(f.daemon.ctx.scoped_launches.is_empty());
        assert_eq!(
            f.daemon.sup.world().turns[&f.issued.turn].task,
            f.issued.task
        );
    }
}

#[test]
fn scoped_runtime_capability_refusal_precedes_the_worker_and_host() {
    let mut f = ScopedFixture::new();
    f.daemon.ctx.current_repositories.test_hire_capability = false;
    assert!(
        !f.daemon
            .ctx
            .repository_allowed(f.daemon.sup.world(), f.issued.agent)
    );
    let Effect::Spawn { launch, .. } = &f.issued.spawn else {
        unreachable!();
    };
    let mut refused = VecDeque::new();
    f.daemon.ctx.prepare_scoped_turn(
        f.issued.turn,
        None,
        launch,
        None,
        &mut refused,
        &f.daemon.sup,
    );
    assert!(
        matches!(refused.pop_front(), Some(Input::LaunchPreparationFailed { turn, .. }) if turn == f.issued.turn)
    );
    assert!(f.daemon.jobs.try_recv().is_err());
    assert_eq!(f.host.spawn_calls(), 0);
}

#[test]
fn claude_scoped_completion_launches_the_original_task_once_with_an_empty_host_install() {
    let mut f = LiveScopedFixture::new();
    let completion = f.completion();
    let prepared = completion.prepared.as_ref().unwrap();
    assert_eq!(
        prepared.launch.spec.stdin,
        dudes_providers::launch::LaunchInput::WriteThenClose(f.task.as_bytes().to_vec())
    );
    assert!(
        prepared
            .launch
            .install
            .directories
            .iter()
            .all(|root| root.is_dir())
    );
    assert!(prepared.launch.install.files.is_empty());
    let spec = prepared.launch.spec.clone();
    let attempt = completion.attempt;
    let outbox = Arc::clone(&completion.outbox);
    f.daemon.complete_scoped_launch(completion).unwrap();
    assert_eq!(f.host.spawn_calls(), 1);
    assert_eq!(f.host.inner.spawned(), [spec]);
    assert_eq!(
        f.host.installs.lock().unwrap().as_slice(),
        [ManagedInstall::default()]
    );
    assert_eq!(f.daemon.sup.world().turns[&f.turn].task, f.task);
    f.daemon
        .complete_scoped_launch(crate::chat_launch::ScopedCompletion {
            attempt,
            prepared: Err("duplicate completion".into()),
            outbox,
        })
        .unwrap();
    assert_eq!(f.host.spawn_calls(), 1);
}

#[test]
fn a_claude_scoped_completion_refuses_a_missing_copy_with_the_original_issue_retained() {
    let mut f = ScopedFixture::new();
    let completion = f.prepare();
    let copy = completion
        .prepared
        .as_ref()
        .unwrap()
        .launch
        .inventory
        .cwd
        .clone();
    std::fs::rename(&copy, copy.with_extension("preserved")).unwrap();
    f.daemon.complete_scoped_launch(completion).unwrap();
    assert_eq!(f.host.spawn_calls(), 0);
    assert!(f.daemon.sup.world().turns[&f.issued.turn].is_issued_not_spawned());
    assert!(
        f.daemon
            .sup
            .world()
            .instructions
            .attempt(f.issued.started)
            .unwrap()
            .finished
            .is_some()
    );
    assert!(f.setup.fixture.records().iter().any(|record| matches!(record,
        Record::InstructionObservationBatchFinished(batch)
            if batch.baseline == (InstructionBaselineRef::Started { lsn: f.issued.started })
                && batch.members.iter().all(|member| matches!(member.settlement,
                    InstructionSettlement::NoProcess { proof: InstructionNoProcessProof::NoHostCall, .. })))));
    assert!(!f.setup.fixture.records().iter().any(
        |record| matches!(record, Record::TurnSpawned { turn, .. } if *turn == f.issued.turn)
    ));
    assert_eq!(
        f.daemon.sup.world().turns[&f.issued.turn].task,
        f.issued.task
    );
    assert_eq!(
        f.daemon.sup.world().instructions.issue_lsn(f.issued.turn),
        Some(f.issued.issue)
    );
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
fn scoped_preparation_failure_keeps_the_issue_and_enters_the_existing_visible_hold() {
    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
    let issued = prepared(&mut daemon, &fixture, Provider::Claude);
    let attempt = crate::codex_profile::scoped::Attempt {
        turn: issued.turn,
        generation: 9,
        started: issued.started,
        owner: issued.owner,
    };
    daemon
        .ctx
        .scoped_launches
        .insert(issued.turn, attempt.into());
    let goals = fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::GoalSet { .. }))
        .count();
    let issue_count = fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::TurnIssued { .. }))
        .count();
    let outbox =
        crate::mailbox::outbox_fixture(&fixture.state.path().join("callback"), issued.agent);
    daemon
        .complete_scoped_launch(crate::chat_launch::ScopedCompletion {
            attempt,
            prepared: Err("scoped preparation refused".into()),
            outbox,
        })
        .unwrap();
    assert_eq!(host.spawn_calls(), 0);
    assert_eq!(daemon.sup.world().turns[&issued.turn].task, issued.task);
    assert!(
        daemon.sup.world().agents[&issued.agent]
            .automatic_hold
            .is_some()
    );
    let records = fixture.records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::GoalSet { .. }))
            .count(),
        goals
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::TurnIssued { .. }))
            .count(),
        issue_count
    );
    assert!(!records.iter().any(|record| matches!(
        record,
        Record::Pause { .. } | Record::Resume { .. } | Record::TurnSpawned { .. }
    )));
}

#[test]
fn stale_scoped_completion_cannot_hold_a_different_physical_attempt() {
    for stale_generation in [false, true] {
        let fixture = common::Fixture::new();
        let host = CountingHost::new();
        let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
        let issued = prepared(&mut daemon, &fixture, Provider::Claude);
        let attempt = crate::codex_profile::scoped::Attempt {
            turn: issued.turn,
            generation: 9,
            started: issued.started,
            owner: issued.owner,
        };
        daemon
            .ctx
            .scoped_launches
            .insert(issued.turn, attempt.into());
        let mut stale = attempt;
        if stale_generation {
            stale.generation += 1;
        } else {
            stale.started = issued.issue;
        }
        if !stale_generation {
            daemon.ctx.scoped_launches.insert(issued.turn, stale.into());
        }
        let before = fixture.records();
        let outbox =
            crate::mailbox::outbox_fixture(&fixture.state.path().join("callback"), issued.agent);
        daemon
            .complete_scoped_launch(crate::chat_launch::ScopedCompletion {
                attempt: stale,
                prepared: Err("stale completion".into()),
                outbox,
            })
            .unwrap();
        assert_eq!(host.spawn_calls(), 0);
        assert_eq!(fixture.records(), before);
        assert_eq!(daemon.sup.world().turns[&issued.turn].task, issued.task);
    }
}

#[test]
fn a_retained_preparation_reservation_cannot_silently_wedge_the_current_attempt() {
    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
    let issued = prepared(&mut daemon, &fixture, Provider::Claude);
    let prior = crate::codex_profile::scoped::Attempt {
        turn: issued.turn,
        generation: 1,
        started: issued.started,
        owner: issued.owner,
    };
    daemon.ctx.scoped_launches.insert(issued.turn, prior.into());
    daemon.ctx.scoped_generation = prior.generation;
    let Effect::Spawn { ref launch, .. } = issued.spawn else {
        panic!("prepared spawn");
    };
    let mut queue = VecDeque::new();
    daemon
        .ctx
        .prepare_scoped_turn(issued.turn, None, launch, None, &mut queue, &daemon.sup);
    assert!(
        matches!(queue.front(), Some(Input::LaunchPreparationFailed { turn, started_lsn, reason }) if *turn == issued.turn && *started_lsn == issued.started && reason.contains("reservation"))
    );
    for input in queue {
        daemon.pump(input).unwrap();
    }
    assert_eq!(host.spawn_calls(), 0);
    assert!(
        daemon.sup.world().agents[&issued.agent]
            .automatic_hold
            .is_some()
    );
    assert_eq!(daemon.sup.world().turns[&issued.turn].task, issued.task);
}

#[test]
fn pending_control_acknowledgements_fold_before_the_scoped_host_check() {
    for control in 0..3 {
        let fixture = common::Fixture::new();
        let host = CountingHost::new();
        let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
        let issued = prepared(&mut daemon, &fixture, Provider::Claude);
        let attempt = crate::codex_profile::scoped::Attempt {
            turn: issued.turn,
            generation: 9,
            started: issued.started,
            owner: issued.owner,
        };
        daemon
            .ctx
            .scoped_launches
            .insert(issued.turn, attempt.into());
        let command = match control {
            0 => Command::Pause {
                agent: issued.agent,
                browser: None,
            },
            1 => Command::Halt {
                agent: issued.agent,
                text: None,
            },
            _ => Command::Retire {
                agent: issued.agent,
                reason: "retire".into(),
                browser_retire: None,
            },
        };
        daemon
            .sup
            .handle(
                Input::Command(command),
                &dudes_supervisor::RecoveryOperation::empty(),
            )
            .unwrap();
        let before = fixture.records();
        let outbox =
            crate::mailbox::outbox_fixture(&fixture.state.path().join("callback"), issued.agent);
        daemon
            .complete_scoped_launch(crate::chat_launch::ScopedCompletion {
                attempt,
                prepared: Err("late preparation".into()),
                outbox,
            })
            .unwrap();
        assert_eq!(host.spawn_calls(), 0);
        let after = fixture.records();
        assert!(after.len() > before.len());
        assert!(after.iter().any(|record| matches!(
            record,
            Record::Pause { .. } | Record::Halt { .. } | Record::AgentRetired { .. }
        )));
        let entries = fixture.entries();
        let control_lsn = entries
            .iter()
            .find_map(|(lsn, entry)| {
                matches!(
                    entry.record,
                    Record::Pause { .. } | Record::Halt { .. } | Record::AgentRetired { .. }
                )
                .then_some(*lsn)
            })
            .unwrap();
        assert!(
            entries
                .iter()
                .filter(|(_, entry)| no_host_finish(&entry.record, &issued, None))
                .all(|(lsn, _)| *lsn > control_lsn)
        );
        assert!(
            !after
                .iter()
                .any(|record| matches!(record, Record::TurnSpawned { .. }))
        );
        assert_eq!(daemon.sup.world().turns[&issued.turn].task, issued.task);
        assert!(daemon.sup.effects().is_empty());
    }
}

#[test]
fn a_failed_scoped_preparation_does_not_clear_uncertain_owned_children() {
    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
    let issued = prepared(&mut daemon, &fixture, Provider::Claude);
    let attempt = crate::codex_profile::scoped::Attempt {
        turn: issued.turn,
        generation: 9,
        started: issued.started,
        owner: issued.owner,
    };
    daemon
        .ctx
        .scoped_launches
        .insert(issued.turn, attempt.into());
    daemon
        .ctx
        .children
        .retain_uncertain(issued.turn, fixture.state.path());
    let outbox =
        crate::mailbox::outbox_fixture(&fixture.state.path().join("callback"), issued.agent);
    daemon
        .complete_scoped_launch(crate::chat_launch::ScopedCompletion {
            attempt,
            prepared: Err("failed preparation".into()),
            outbox,
        })
        .unwrap();
    assert_eq!(host.spawn_calls(), 0);
    assert_eq!(daemon.sup.world().turns[&issued.turn].task, issued.task);
    assert!(
        daemon
            .ctx
            .admit_prior_attempt(&daemon.sup, issued.agent, Some(issued.turn))
            .is_err()
    );
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
    let root = fixture.state.path().canonicalize().unwrap();
    let other = fixture.runtime.path().canonicalize().unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
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
    assert!(daemon.ask(Ask::Resume(attempt.agent)).is_ok());
    assert_eq!(
        daemon.sup.world().agents[&attempt.agent].repositories,
        original
    );
    assert_eq!(daemon.sup.world().turns[&attempt.turn].task, attempt.task);
    assert_eq!(host.spawn_calls(), 0);
}

#[test]
fn a_failed_catalog_append_rolls_an_applied_reload_back() {
    use crate::floor_actions::ConfiguredRepository;
    use dudes_proto::repository::{RepoId, RepositoryEntry, RepositorySet};

    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
    let root = fixture.state.path().canonicalize().unwrap();
    let other = fixture.runtime.path().canonicalize().unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::create_dir(other.join(".git")).unwrap();
    daemon.ctx.enrollment = legacy_enrollment(root.clone());
    daemon.observe_repository_enrollment().unwrap();
    daemon.record_missing_project_catalogs().unwrap();
    assert!(
        fixture
            .records()
            .iter()
            .all(|record| !matches!(record, Record::ProjectRepositoriesRecorded { .. }))
    );
    let installed = daemon.ctx.enrollment.clone();
    let mut candidate = installed.clone();
    let member = |id: &str, path: &PathBuf| RepositoryEntry {
        repo_id: RepoId::parse(id).unwrap(),
        value: ConfiguredRepository {
            label: id.into(),
            path: path.clone(),
            written_path: path.clone(),
        },
    };
    candidate.profiles[0].repositories = Some(
        RepositorySet::new(
            RepoId::parse("primary").unwrap(),
            vec![member("primary", &root), member("other", &other)],
        )
        .unwrap(),
    );
    let sha256 = "c".repeat(64);
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
    let before = fixture.entries();
    daemon
        .append_failure()
        .fail_nth(std::num::NonZeroUsize::new(1).unwrap());
    let refusal = daemon.enrollment_reload(&read).unwrap_err();
    assert!(
        refusal.message.contains("could not be observed"),
        "{}",
        refusal.message
    );
    assert_eq!(fixture.entries(), before);
    assert!(daemon.ctx.enrollment.profiles[0].repositories.is_none());
    assert!(daemon.enrollment_reload(&read).unwrap().applied);
    let recorded = fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::ProjectRepositoriesRecorded { .. }))
        .count();
    assert_eq!(recorded, 1);
    assert!(
        daemon.ctx.current_repositories.observed["service"]
            .lsn
            .is_some()
    );
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

#[test]
fn a_final_repository_refusal_is_visible_and_operator_resume_can_issue_work() {
    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
    let root = fixture.state.path().join("project");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let root = root.canonicalize().unwrap();
    daemon.ctx.enrollment = legacy_enrollment(root.clone());
    daemon.observe_repository_enrollment().unwrap();
    let attempt = prepared(&mut daemon, &fixture, Provider::Claude);
    let saved = root.with_file_name("preserved-project");
    let hidden = root.clone();
    let preserved = saved.clone();
    daemon.ctx.before_recovery_dispatch = Some(Box::new(move || {
        std::fs::rename(hidden, preserved).unwrap();
    }));
    let mut queue = VecDeque::new();
    execute(&mut daemon, &attempt.spawn, &mut queue);
    while let Some(input) = queue.pop_front() {
        daemon.pump(input).unwrap();
    }
    assert_eq!(host.spawn_calls(), 0);
    let visible = matches!(
        daemon.sup.world().agents[&attempt.agent].automatic_hold,
        Some(AutomaticHold::LaunchFailed { .. })
    );
    std::fs::rename(saved, &root).unwrap();
    daemon.pump(Input::Tick).unwrap();
    assert_eq!(
        host.spawn_calls(),
        0,
        "restoration alone cannot resume a visible failure"
    );
    daemon.ask(Ask::Resume(attempt.agent)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while host.spawn_calls() == 0 && std::time::Instant::now() < deadline {
        if let Ok(Job::Input(input)) = daemon.jobs.recv_timeout(Duration::from_millis(20)) {
            daemon.pump_runtime_input(*input).unwrap();
        }
    }
    assert_eq!(
        host.spawn_calls(),
        1,
        "the late refusal silently retained its spawning claim"
    );
    assert!(
        visible,
        "the no-host refusal must expose an operator-resumable failure"
    );
    assert!(daemon.sup.world().turns[&attempt.turn].has_ended());
    assert!(
        fixture
            .records()
            .iter()
            .any(|record| no_host_finish(record, &attempt, None))
    );
}

fn finite_spend_snapshot(
    daemon: &TestDaemon,
    agent: AgentId,
) -> (dudes_journal::SpendViews, [dudes_proto::Spend; 2]) {
    let views = daemon
        .ctx
        .projections
        .conversation_spend_views(agent, daemon.ctx.clock.now().utc_day())
        .unwrap();
    let per_turn = daemon.sup.config().per_turn_floor;
    (
        views,
        [
            dudes_policy::budget::reserved(
                views.agent_today,
                views.agent_unknown_ends_today,
                per_turn,
            ),
            dudes_policy::budget::reserved(
                views.floor_today,
                views.floor_unknown_ends_today,
                per_turn,
            ),
        ],
    )
}

fn queue_finite_work(
    daemon: &mut TestDaemon,
    agent: AgentId,
    session: &crate::floor_session::SessionIdentity,
    request: u8,
) {
    let context_lsn = daemon.sup.world().agents[&agent].context_lsn;
    daemon
        .pump_runtime_input(Input::Command(Command::ConversationMessageQueued {
            agent,
            text: format!("Perform finite request {request}"),
            context_lsn,
            context_reset_turn: None,
            browser: dudes_proto::BrowserNoteAttribution {
                request_id_digest: dudes_proto::record::Hex32::new([request; 32]),
                request_digest: dudes_proto::record::Hex32::new([request; 32]),
                session_digest: session.digest(),
            },
        }))
        .unwrap();
}

fn after_one_unpriced_finite_end(
    before: dudes_journal::SpendViews,
    per_turn: dudes_proto::Spend,
) -> (dudes_journal::SpendViews, [dudes_proto::Spend; 2]) {
    let mut failure = before;
    failure.agent_unknown_ends_today = failure.agent_unknown_ends_today.saturating_add(1);
    failure.floor_unknown_ends_today = failure.floor_unknown_ends_today.saturating_add(1);
    (
        failure,
        [
            dudes_policy::budget::reserved(
                failure.agent_today,
                failure.agent_unknown_ends_today,
                per_turn,
            ),
            dudes_policy::budget::reserved(
                failure.floor_today,
                failure.floor_unknown_ends_today,
                per_turn,
            ),
        ],
    )
}

#[test]
fn a_final_finite_repository_refusal_uses_standard_failure_accounting() {
    let fixture = common::Fixture::new();
    let host = CountingHost::new();
    let mut daemon = boot(&fixture, &host, Provider::Claude, common::true_program());
    let root = fixture.state.path().join("project");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let root = root.canonicalize().unwrap();
    daemon.ctx.enrollment = legacy_enrollment(root.clone());
    daemon.observe_repository_enrollment().unwrap();
    let Answered::Hired(hired) = daemon
        .ask(Ask::Hire(Box::new(common::hire_params(&root))))
        .unwrap()
    else {
        unreachable!()
    };
    let agent = hired.agent;
    let sessions = crate::floor_session::FloorSessions::new(
        "127.0.0.1:31337".parse().unwrap(),
        Arc::clone(&daemon.ctx.clock),
    );
    let session = sessions
        .admitted_for_test("127.0.0.1:31337", "http://127.0.0.1:31337")
        .0;
    daemon.ctx.browser_session = Some(session.clone());
    let before = finite_spend_snapshot(&daemon, agent);
    let expected = after_one_unpriced_finite_end(before.0, daemon.sup.config().per_turn_floor);
    let saved = root.with_file_name("preserved-project");
    let hidden = root.clone();
    let preserved = saved.clone();
    daemon.ctx.before_recovery_dispatch = Some(Box::new(move || {
        std::fs::rename(hidden, preserved).unwrap();
    }));
    queue_finite_work(&mut daemon, agent, &session, 7);
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
    assert!(matches!(
        daemon.sup.world().agents[&agent].automatic_hold,
        Some(AutomaticHold::LaunchFailed { .. })
    ));
    let failed = fixture
        .records()
        .iter()
        .find_map(|record| {
            record
                .turn_issue()
                .filter(|issue| issue.agent == agent)
                .map(|issue| issue.turn)
        })
        .unwrap();
    assert!(daemon.sup.world().turns[&failed].has_ended());
    assert_eq!(fixture.records().iter().filter(|record| matches!(record,
        Record::TurnEnded { turn, usage: None, outcome: Some(TurnOutcome::NotLaunched { .. }), .. } if *turn == failed)).count(), 1);
    assert_eq!(
        daemon
            .sup
            .world()
            .browser_issue(failed)
            .unwrap()
            .dispatch_intents
            .len(),
        1
    );
    assert_eq!(finite_spend_snapshot(&daemon, agent), expected);
    std::fs::rename(saved, root).unwrap();
    daemon.pump(Input::Tick).unwrap();
    assert_eq!(host.spawn_calls(), 0);
    daemon.ask(Ask::Resume(agent)).unwrap();
    assert_eq!(
        host.spawn_calls(),
        0,
        "Resume does not invent a new conversation request"
    );
    queue_finite_work(&mut daemon, agent, &session, 8);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while host.spawn_calls() == 0 && std::time::Instant::now() < deadline {
        if let Ok(Job::Input(input)) = daemon.jobs.recv_timeout(Duration::from_millis(20)) {
            daemon.pump_runtime_input(*input).unwrap();
        }
    }
    assert_eq!(host.spawn_calls(), 1, "new operator work runs after Resume");
    assert_eq!(finite_spend_snapshot(&daemon, agent), expected);
    assert!(
        fixture
            .records()
            .iter()
            .all(|record| !matches!(record, Record::ConversationDispatchUncertain { .. }))
    );
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
    let Effect::Spawn {
        launch: dudes_supervisor::ManagedSpawn::Composed { spec, .. },
        ..
    } = &mut attempt.spawn
    else {
        unreachable!()
    };
    assert_eq!(spec.argv.get(2), Some(&OsString::from("--add-dir")));
    spec.argv[2] = "--missing-owned-outbox-option".into();
    crate::program::resolve_launch(spec.as_ref().clone())
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
