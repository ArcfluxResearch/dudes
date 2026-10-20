use super::*;
use crate::floor_session::{FloorSessions, SessionIdentity};
use crate::settings::{FloorControl, FloorSettings};
use dudes_proto::record::{Hex32, samples};
use dudes_proto::rpc::recovery::{RecoveryChoice as WireChoice, RecoverySubmitParams};
use dudes_proto::{Record, RecordKind};
use dudes_types::{AgentId, GoalId, Lsn, Mode, ModelSelection, Provenance, TurnId};
use std::collections::BTreeMap;

mod invalidated_progress;

struct BrowserFixture {
    fixture: common::Fixture,
    daemon: TestDaemon,
    sessions: FloorSessions,
    identity: SessionIdentity,
    intent: RecoverySubmitParams,
    clock: Arc<common::TestClock>,
}

fn source_hire(daemon: &mut TestDaemon, fixture: &common::Fixture, agent: AgentId) {
    let mut record = samples::record(RecordKind::AgentHired);
    let Record::AgentHired {
        id,
        cwd,
        mcp_servers,
        ..
    } = &mut record
    else {
        unreachable!()
    };
    *id = agent;
    *cwd = fixture.state.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    mcp_servers.clear();
    append_history(daemon, record);
}

fn source_goal(daemon: &mut TestDaemon, agent: AgentId, id: GoalId) {
    let mut record = samples::record(RecordKind::GoalSet);
    let Record::GoalSet {
        agent: owner, goal, ..
    } = &mut record
    else {
        unreachable!()
    };
    *owner = agent;
    goal.id = id;
    goal.text = "finish the owned fixture".into();
    goal.predicate = dudes_proto::Predicate::Command {
        argv: vec!["false".into()],
        timeout_s: 1,
    };
    goal.completion_check = None;
    goal.baseline = None;
    goal.hop_cap = 20;
    goal.budget = dudes_proto::GoalBudget {
        usd: None,
        tokens: None,
    };
    append_history(daemon, record);
}

fn failed_source(daemon: &mut TestDaemon, fixture: &common::Fixture) -> AgentId {
    failed_source_at(daemon, fixture, 915)
}

fn failed_source_at(daemon: &mut TestDaemon, fixture: &common::Fixture, base: u64) -> AgentId {
    let agent = issued_source_at(daemon, fixture, base);
    let turn = TurnId::from_parts(base, 3);
    append_history(
        daemon,
        Record::TurnEnded {
            agent,
            turn,
            mode: Mode::Managed,
            provenance: Provenance::Observed,
            exit: dudes_proto::ExitStatus::UNKNOWN,
            usage: None,
            model_usage: BTreeMap::default(),
            last_message: None,
            session: None,
            outcome: Some(dudes_proto::TurnOutcome::NotLaunched {
                reason: "owned fixture never called a host".into(),
            }),
            elapsed_ms: None,
            final_reply: None,
            session_usage: None,
        },
    );
    append_history(
        daemon,
        Record::AgentHeld {
            agent,
            provenance: Provenance::Observed,
            cause: dudes_proto::AutomaticHold::LaunchFailed {
                reason: "owned fixture never called a host".into(),
            },
        },
    );
    agent
}

fn vanished_source(daemon: &mut TestDaemon, fixture: &common::Fixture) -> AgentId {
    let agent = issued_source_at(daemon, fixture, 918);
    let turn = TurnId::from_parts(918, 3);
    let mut spawn = samples::record(RecordKind::TurnSpawned);
    let Record::TurnSpawned {
        agent: owner,
        turn: issued,
        ..
    } = &mut spawn
    else {
        unreachable!()
    };
    *owner = agent;
    *issued = turn;
    append_history(daemon, spawn);
    append_history(
        daemon,
        Record::TurnEnded {
            agent,
            turn,
            mode: Mode::Managed,
            provenance: Provenance::Observed,
            exit: dudes_proto::ExitStatus::UNKNOWN,
            usage: None,
            model_usage: BTreeMap::default(),
            last_message: None,
            session: None,
            outcome: Some(dudes_proto::TurnOutcome::Vanished),
            elapsed_ms: None,
            final_reply: None,
            session_usage: None,
        },
    );
    agent
}

fn issued_source_at(daemon: &mut TestDaemon, fixture: &common::Fixture, base: u64) -> AgentId {
    let agent = AgentId::from_parts(base, 1);
    let goal = GoalId::from_parts(base, 2);
    let turn = TurnId::from_parts(base, 3);
    source_hire(daemon, fixture, agent);
    source_goal(daemon, agent, goal);
    let decision = daemon.ctx.writer.next_lsn().unwrap();
    append_history(
        daemon,
        Record::LoopDecided {
            agent,
            input_digest: "12".repeat(32),
            decision: dudes_proto::LoopDecision::Continue {
                task: "original owned task".into(),
            },
            gate_trace: Vec::new(),
        },
    );
    append_history(
        daemon,
        Record::TurnIssued {
            agent,
            goal,
            turn,
            task: "original owned task".into(),
            session: None,
            mode: Mode::Managed,
            model_selection: Some(ModelSelection::provider_default()),
            lsn_of_decision: decision,
            answers: Vec::new(),
        },
    );
    agent
}

fn wait_for_human(daemon: &mut TestDaemon, agent: AgentId) {
    let mut question = samples::record(RecordKind::HumanQuestion);
    let Record::HumanQuestion { agent: owner, .. } = &mut question else {
        unreachable!()
    };
    *owner = agent;
    append_history(daemon, question);
}

fn sign_in(sessions: &FloorSessions, host: &str) -> SessionIdentity {
    let origin = format!("http://{host}");
    let bootstrap = sessions.issue().unwrap();
    let exchange = sessions
        .exchange(host, &origin, bootstrap.bootstrap.as_bytes())
        .unwrap();
    sessions
        .authorize_onboard(host, &origin, &exchange.cookie, &exchange.csrf)
        .unwrap()
}

fn inspected_intent(daemon: &mut TestDaemon, agent: AgentId, request: u8) -> RecoverySubmitParams {
    let inspected = daemon
        .recovery_inspect(agent, operator_admission())
        .unwrap();
    let Answered::RecoveryInspected(RecoveryInspectResult::Available { expected, .. }) = inspected
    else {
        panic!(
            "the owned failed source must be inspectable: {inspected:?}; pure={:?}; unresolved={:?}",
            daemon.sup.recovery_opportunity(agent),
            daemon
                .sup
                .world()
                .instructions
                .unresolved_launch_attempts(agent)
                .collect::<Vec<_>>()
        )
    };
    RecoverySubmitParams {
        request_id: Hex32::new([request; 32]),
        expected: *expected,
        choice: WireChoice::FreshWithRecap,
    }
}

fn browser_fixture() -> BrowserFixture {
    browser_fixture_with_checkpoints(0)
}

fn browser_fixture_with_checkpoints(checkpoints: u64) -> BrowserFixture {
    browser_fixture_from_source(checkpoints, failed_source)
}

fn browser_fixture_from_source(
    checkpoints: u64,
    source: fn(&mut TestDaemon, &common::Fixture) -> AgentId,
) -> BrowserFixture {
    let fixture = common::Fixture::new();
    let settings = floor_settings(&fixture, checkpoints);
    let clock = Arc::new(common::TestClock::ticking(1_787_394_600_000, 0));
    let mut daemon = boot_at(&settings, &clock);
    let agent = source(&mut daemon, &fixture);
    signed_fixture(fixture, daemon, agent, clock)
}

fn floor_settings(fixture: &common::Fixture, checkpoints: u64) -> crate::settings::Settings {
    let mut settings = fixture.settings();
    settings.journal.checkpoint_every = checkpoints;
    settings.floor_control = FloorControl::On {
        goal: false,
        retire: false,
        renew: false,
    };
    settings.floor_recovery_enabled = true;
    settings.floor = Some(FloorSettings {
        assets_dir: std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/floor-release-v3"
        )),
        port: Some(0),
    });
    settings
}

fn boot_at(settings: &crate::settings::Settings, clock: &Arc<common::TestClock>) -> TestDaemon {
    crate::boot(
        settings,
        common::seams(
            settings,
            common::SharedHost::new(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::clone(clock),
            Witness::default(),
        ),
    )
    .unwrap()
}

fn signed_fixture(
    fixture: common::Fixture,
    mut daemon: TestDaemon,
    agent: AgentId,
    clock: Arc<common::TestClock>,
) -> BrowserFixture {
    let floor = daemon.floor.as_ref().unwrap();
    let sessions = floor
        .carry(crate::baseline::CheckRuns::default())
        .sessions()
        .clone();
    let host = floor.addr().to_string();
    let identity = sign_in(&sessions, &host);
    let intent = inspected_intent(&mut daemon, agent, 121);
    BrowserFixture {
        fixture,
        daemon,
        sessions,
        identity,
        intent,
        clock,
    }
}

fn command_frames(fixture: &common::Fixture, since: usize) -> Vec<RecordKind> {
    fixture
        .entries()
        .into_iter()
        .skip(since)
        .map(|(_, entry)| entry.record.kind())
        .filter(|kind| *kind != RecordKind::Checkpoint)
        .collect()
}

fn invalidate_without_a_retained_registry_guard(sessions: &FloorSessions) {
    let sessions = sessions.clone();
    let (completed, waiting) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        sessions.invalidate();
        completed.send(()).unwrap();
    });
    waiting
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("the registry lock is not retained here");
    worker.join().unwrap();
}

fn clean_before_write_failure(owned: &mut BrowserFixture, kind: RecordKind) {
    let before = owned.fixture.entries().len();
    let work = owned.intent.expected.work;
    let original = owned.daemon.sup.world().turns[&work.turn].clone();
    let sessions = owned.sessions.clone();
    owned.daemon.ctx.before_recovery_append = Some(Box::new(move |record| {
        if record.kind() == kind {
            invalidate_without_a_retained_registry_guard(&sessions);
        }
    }));
    let submitted = owned
        .daemon
        .recovery_submit_browser(owned.identity.clone(), &owned.intent);
    let RecoverySubmitResult::Refused { reason, prefix } = submitted else {
        panic!("{kind:?}: {submitted:?}")
    };
    assert_eq!(reason, RecoverySubmitRefusal::AuthorityUnavailable);
    assert_eq!(
        prefix.required.is_some(),
        kind != RecordKind::RecoveryRequired
    );
    assert_eq!(
        prefix.resume.is_some(),
        kind == RecordKind::RecoveryAuthorized
    );
    let expected = match kind {
        RecordKind::RecoveryRequired => vec![],
        RecordKind::Resume => vec![RecordKind::RecoveryRequired],
        RecordKind::RecoveryAuthorized => vec![RecordKind::RecoveryRequired, RecordKind::Resume],
        _ => unreachable!(),
    };
    assert_eq!(command_frames(&owned.fixture, before), expected);
    assert!(owned.daemon.ctx.host.spawned().is_empty());
    assert!(!owned.daemon.ctx.handle.journal_uncertain());
    assert!(owned.daemon.ctx.writer.poisoned().is_none());
    assert!(
        owned
            .daemon
            .ctx
            .writer
            .checked_recovery(&mut owned.daemon.ctx.projections)
            .is_ok()
    );
    assert!(matches!(
        owned.daemon.ctx.recovery_read(owned.intent.request_id),
        RecoveryReceiptResult::Absent { .. }
    ));
    let after = &owned.daemon.sup.world().turns[&work.turn];
    assert_eq!(
        (&after.task, &after.answers, after.issue_lsn),
        (&original.task, &original.answers, original.issue_lsn)
    );
}

#[test]
fn each_command_owned_append_acquires_its_own_original_session_permit() {
    for kind in [
        RecordKind::RecoveryRequired,
        RecordKind::Resume,
        RecordKind::RecoveryAuthorized,
    ] {
        let mut owned = browser_fixture_with_checkpoints(1);
        clean_before_write_failure(&mut owned, kind);
        let mut running = common::start(owned.daemon);
        running.stop(crate::ExitCause::Ended);
    }
}

#[test]
fn missing_auxiliary_registry_is_indeterminate_and_does_not_invalidate_the_session() {
    let mut owned = browser_fixture();
    let before = owned.fixture.entries();
    owned.daemon.ctx.recovery_authority = None;
    let submitted = owned
        .daemon
        .recovery_submit_browser(owned.identity.clone(), &owned.intent);
    assert_eq!(
        submitted,
        RecoverySubmitResult::Refused {
            reason: RecoverySubmitRefusal::Unavailable,
            prefix: RecoveryKnownPrefix {
                required: None,
                resume: None
            },
        }
    );
    assert_eq!(owned.fixture.entries(), before);
    let origin = RecoveryAttribution::BrowserSession {
        session_digest: owned.identity.digest(),
    };
    assert_eq!(
        owned.sessions.recovery_authority().observe_original(
            &origin,
            crate::floor_session::RecoveryGrants {
                recovery: true,
                pause_resume: true
            }
        ),
        dudes_supervisor::RecoveryAvailability::Live
    );
    assert!(!owned.daemon.ctx.handle.journal_uncertain());
    let mut running = common::start(owned.daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn authority_loss_after_authorization_fsync_is_observed_again_before_its_acknowledgment() {
    let mut owned = browser_fixture();
    let sessions = owned.sessions.clone();
    owned.daemon.ctx.after_recovery_fsync = Some(Box::new(move |record| {
        if matches!(record, Record::RecoveryAuthorized(_)) {
            invalidate_without_a_retained_registry_guard(&sessions);
        }
    }));
    let submitted = owned
        .daemon
        .recovery_submit_browser(owned.identity.clone(), &owned.intent);
    let RecoverySubmitResult::Accepted { receipt } = submitted else {
        panic!("{submitted:?}")
    };
    assert!(matches!(
        receipt.status,
        RecoveryReceiptState::Invalidated {
            reason: RecoveryReceiptInvalidation::AuthorityUnavailable,
            ..
        }
    ));
    assert!(owned.daemon.ctx.host.spawned().is_empty());
    let entries = owned.fixture.entries();
    let authorization = entries
        .iter()
        .find_map(|(_, entry)| match &entry.record {
            Record::RecoveryAuthorized(authorization) => Some(authorization),
            _ => None,
        })
        .unwrap();
    let RecoveryPlan::New { turn, .. } = authorization.plan else {
        unreachable!()
    };
    assert!(!entries.iter().any(|(_, entry)| matches!(entry.record, Record::TurnIssued { turn: issued, .. } | Record::TurnSpawned { turn: issued, .. } | Record::TurnEnded { turn: issued, .. } if issued == turn)));
    assert!(!owned.daemon.ctx.handle.journal_uncertain());
    let mut running = common::start(owned.daemon);
    running.stop(crate::ExitCause::Ended);
}

fn read_receipt(handle: &crate::Handle, request: Hex32) -> RecoveryReceiptView {
    let result = dudes_rpc::methods::recovery::RecoveryControl::recovery_receipt(
        handle,
        dudes_proto::rpc::recovery::RecoveryReceiptParams {
            request_id: request,
        },
        operator_admission(),
    )
    .unwrap();
    let RecoveryReceiptResult::Found { receipt } = result else {
        panic!("receipt={result:?}")
    };
    *receipt
}

fn authorized_turn(fixture: &common::Fixture, request: Hex32) -> TurnId {
    fixture.await_record("the exact reserved recovery turn", |record| match record {
        Record::RecoveryAuthorized(authorization) if authorization.request_id == request => {
            match authorization.plan {
                RecoveryPlan::New { turn, .. } => Some(turn),
                RecoveryPlan::RepeatIssued { ref issue, .. } => Some(issue.issued.work.turn),
            }
        }
        _ => None,
    })
}

#[test]
fn delayed_new_issue_rechecks_expiry_and_cancels_only_its_unwritten_launch() {
    for continued_boundary in [false, true] {
        let mut owned = if continued_boundary {
            browser_fixture_from_source(0, vanished_source)
        } else {
            browser_fixture()
        };
        let clock = Arc::clone(&owned.clock);
        let (reached, waiting) = std::sync::mpsc::channel();
        owned.daemon.ctx.before_recovery_append = Some(Box::new(move |record| {
            if matches!(record, Record::TurnIssued { .. }) {
                clock.advance(24 * 60 * 60_000);
                reached.send(()).unwrap();
            }
        }));
        let host = owned.daemon.ctx.host.clone();
        let mut running = common::start(owned.daemon);
        let submitted = running
            .handle
            .submit_floor_recovery(owned.identity.clone(), &owned.intent);
        assert!(
            matches!(submitted, RecoverySubmitResult::Accepted { ref receipt } if receipt.status == RecoveryReceiptState::Pending)
        );
        waiting
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let receipt = read_receipt(&running.handle, owned.intent.request_id);
        assert_eq!(receipt.status, RecoveryReceiptState::Pending);
        let turn = authorized_turn(&owned.fixture, owned.intent.request_id);
        let entries = owned.fixture.entries();
        assert!(!entries.iter().any(|(_, entry)| matches!(entry.record, Record::TurnIssued { turn: actual, .. } | Record::TurnSpawned { turn: actual, .. } | Record::TurnEnded { turn: actual, .. } if actual == turn)));
        assert!(entries.iter().any(|(_, entry)| matches!(
            entry.record,
            Record::InstructionObservationStarted { .. }
        )));
        assert!(entries.iter().any(
            |(_, entry)| matches!(&entry.record, Record::InstructionObservationStarted {
        owner: dudes_proto::InstructionObservationOwner::Managed { turn: actual, .. }, snapshot, ..
    } if *actual == turn && snapshot.execution_folder.is_some())
        ));
        assert!(host.spawned().is_empty());
        running.handle.input(dudes_supervisor::Input::Tick);
        let invalidated = read_receipt(&running.handle, owned.intent.request_id);
        assert!(matches!(
            invalidated.status,
            RecoveryReceiptState::Invalidated {
                reason: RecoveryReceiptInvalidation::AuthorityUnavailable,
                ..
            }
        ));
        assert!(host.spawned().is_empty());
        assert!(!owned.fixture.records().iter().any(
            |record| matches!(record, Record::TurnEnded { turn: actual, .. } if *actual == turn)
        ));
        running.stop(crate::ExitCause::Ended);
        continue_unwritten_after_replay(
            owned.fixture,
            owned.clock,
            &owned.intent,
            turn,
            continued_boundary,
        );
    }
}

fn continue_unwritten_after_replay(
    fixture: common::Fixture,
    clock: Arc<common::TestClock>,
    intent: &RecoverySubmitParams,
    turn: TurnId,
    continued_boundary: bool,
) {
    if !continued_boundary {
        let mut resumed = boot_at(&floor_settings(&fixture, 0), &clock);
        assert!(matches!(
            resumed
                .recovery_inspect(intent.expected.work.agent, operator_admission())
                .unwrap(),
            Answered::RecoveryInspected(RecoveryInspectResult::Unavailable {
                reason: dudes_proto::rpc::recovery::RecoveryInspectUnavailable::NotRecoverable
            })
        ));
        assert!(resumed.ctx.host.spawned().is_empty());
        let mut running = common::start(resumed);
        running.stop(crate::ExitCause::Ended);
        return;
    }
    let settings = floor_settings(&fixture, 0);
    let daemon = boot_at(&settings, &clock);
    let mut resumed = signed_fixture(fixture, daemon, intent.expected.work.agent, clock);
    resumed.intent.request_id = Hex32::new([125; 32]);
    assert_eq!(resumed.intent.expected.work, intent.expected.work);
    let host = resumed.daemon.ctx.host.clone();
    let mut running = common::start(resumed.daemon);
    let submitted = running
        .handle
        .submit_floor_recovery(resumed.identity, &resumed.intent);
    assert!(
        matches!(submitted, RecoverySubmitResult::Accepted { .. }),
        "{submitted:?}"
    );
    let continued = await_consumed(&resumed.fixture, &running.handle, resumed.intent.request_id);
    assert!(matches!(
        continued.status,
        RecoveryReceiptState::Consumed { .. }
    ));
    assert_ne!(
        authorized_turn(&resumed.fixture, resumed.intent.request_id),
        turn
    );
    assert_eq!(host.spawned().len(), 1);
    assert!(!resumed.fixture.records().iter().any(|record| matches!(record, Record::TurnIssued { turn: actual, .. } | Record::TurnSpawned { turn: actual, .. } | Record::TurnEnded { turn: actual, .. } if *actual == turn)));
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn physical_checkpoint_fsync_releases_the_permit_before_projection_and_the_next_append() {
    let mut owned = browser_fixture_with_checkpoints(1);
    let sessions = owned.sessions.clone();
    let journal = owned.fixture.journal_dir();
    let (observed, waiting) = std::sync::mpsc::channel();
    owned.daemon.ctx.after_recovery_fsync = Some(Box::new(move |record| {
        if matches!(record, Record::RecoveryRequired(_)) {
            let entries = dudes_journal::Reader::new(&journal)
                .iter_from(Lsn::ZERO)
                .unwrap()
                .collect::<dudes_journal::Result<Vec<_>>>()
                .unwrap();
            assert!(matches!(
                entries.last().unwrap().1.record,
                Record::Checkpoint { .. }
            ));
            invalidate_without_a_retained_registry_guard(&sessions);
            observed.send(()).unwrap();
        }
    }));
    let submitted = owned
        .daemon
        .recovery_submit_browser(owned.identity.clone(), &owned.intent);
    waiting
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    let RecoverySubmitResult::Refused { reason, prefix } = submitted else {
        panic!("{submitted:?}")
    };
    assert_eq!(reason, RecoverySubmitRefusal::AuthorityUnavailable);
    assert!(prefix.required.is_some());
    assert!(prefix.resume.is_none());
    assert!(!owned.daemon.ctx.handle.journal_uncertain());
    assert!(owned.daemon.ctx.host.spawned().is_empty());
    let mut running = common::start(owned.daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn post_fsync_uncertainty_is_unknown_and_never_completes_as_no_write() {
    let mut owned = browser_fixture_with_checkpoints(1);
    owned
        .daemon
        .ctx
        .onboarding_failure
        .arm(crate::runtime::OnboardingFailurePoint::Control);
    let submitted = owned
        .daemon
        .recovery_submit_browser(owned.identity.clone(), &owned.intent);
    assert_eq!(
        submitted,
        RecoverySubmitResult::Unknown {
            request_id: owned.intent.request_id
        }
    );
    assert!(owned.fixture.records().iter().any(|record| matches!(record, Record::Resume { browser: Some(browser), .. } if browser.request_id == owned.intent.request_id)));
    assert!(
        !owned
            .fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::RecoveryAuthorized(_)))
    );
    assert!(owned.daemon.ctx.handle.journal_uncertain());
    assert!(owned.daemon.ctx.host.spawned().is_empty());
    assert!(matches!(
        owned.daemon.ctx.recovery_read(owned.intent.request_id),
        RecoveryReceiptResult::Unavailable {
            reason: RecoveryReadUnavailable::JournalUncertain
        }
    ));
    let mut running = common::start(owned.daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn expiry_after_preparation_holds_the_durable_issue_without_a_host_or_terminal_result() {
    let mut owned = browser_fixture();
    let clock = Arc::clone(&owned.clock);
    let (prepared, waiting) = std::sync::mpsc::channel();
    owned.daemon.ctx.before_recovery_dispatch = Some(Box::new(move || {
        clock.advance(24 * 60 * 60_000);
        prepared.send(()).unwrap();
    }));
    let host = owned.daemon.ctx.host.clone();
    let host_name = owned.daemon.floor_addr().unwrap().to_string();
    let mut running = common::start(owned.daemon);
    let submitted = running
        .handle
        .submit_floor_recovery(owned.identity.clone(), &owned.intent);
    assert!(
        matches!(submitted, RecoverySubmitResult::Accepted { ref receipt } if receipt.status == RecoveryReceiptState::Pending)
    );
    waiting
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let receipt = read_receipt(&running.handle, owned.intent.request_id);
    let RecoveryReceiptState::IssueBound { turn, issue_lsn } = receipt.status else {
        panic!("{receipt:?}")
    };
    assert_eq!(
        turn,
        authorized_turn(&owned.fixture, owned.intent.request_id)
    );
    let before = owned.fixture.entries();
    let issue = before
        .iter()
        .find_map(|(lsn, entry)| (*lsn == issue_lsn).then_some(entry.clone()))
        .unwrap();
    assert!(
        matches!(&issue.record, Record::TurnIssued { turn: actual, task, .. } if *actual == turn && !task.is_empty())
    );
    assert!(host.spawned().is_empty());
    assert!(!before.iter().any(|(_, entry)| matches!(entry.record, Record::TurnSpawned { turn: actual, .. } | Record::TurnEnded { turn: actual, .. } if actual == turn)));
    let inspection = dudes_rpc::methods::recovery::RecoveryControl::inspect_recovery(
        &running.handle,
        dudes_proto::rpc::recovery::RecoveryInspectParams {
            agent: owned.intent.expected.work.agent,
        },
        operator_admission(),
    )
    .unwrap();
    assert!(matches!(
        inspection,
        RecoveryInspectResult::Unavailable { .. }
    ));
    let new_identity = sign_in(&owned.sessions, &host_name);
    let mut distinct = owned.intent;
    distinct.request_id = Hex32::new([122; 32]);
    distinct.choice = WireChoice::RepeatIssuedFresh;
    distinct.expected.work.turn = turn;
    distinct.expected.issue_lsn = issue_lsn;
    distinct.expected.source_lsn = issue_lsn;
    distinct.expected.episode = None;
    distinct.expected.expected_predecessor = Some(receipt.claim);
    distinct.expected.control_lsn = before
        .iter()
        .find_map(|(_, entry)| match &entry.record {
            Record::RecoveryAuthorized(authorization)
                if authorization.request_id == owned.intent.request_id =>
            {
                Some(authorization.resulting_control)
            }
            _ => None,
        })
        .unwrap();
    let refused = running
        .handle
        .submit_floor_recovery(new_identity, &distinct);
    assert!(matches!(
        refused,
        RecoverySubmitResult::Refused {
            reason: RecoverySubmitRefusal::NotRecoverable,
            ..
        }
    ));
    assert_eq!(owned.fixture.entries(), before);
    assert_eq!(
        read_receipt(&running.handle, owned.intent.request_id),
        receipt
    );
    assert!(host.spawned().is_empty());
    running.stop(crate::ExitCause::Ended);
    repeat_held_after_real_replay(
        &owned.fixture,
        &owned.clock,
        owned.intent.expected.work,
        owned.intent.request_id,
        &receipt,
        &issue,
    );
}

fn repeat_held_after_real_replay(
    fixture: &common::Fixture,
    clock: &Arc<common::TestClock>,
    work: dudes_proto::recovery::RecoveryWork,
    request: Hex32,
    receipt: &RecoveryReceiptView,
    issue: &dudes_proto::Entry,
) {
    let RecoveryReceiptState::IssueBound { turn, issue_lsn } = receipt.status else {
        unreachable!()
    };
    let settings = floor_settings(fixture, 0);
    let mut resumed = boot_at(&settings, clock);
    let row = &resumed.sup.world().turns[&turn];
    let Record::TurnIssued { task, answers, .. } = &issue.record else {
        unreachable!()
    };
    assert_eq!(
        (&row.task, &row.answers, row.issue_lsn),
        (task, answers, issue_lsn)
    );
    assert_eq!(resumed.sup.world().agents[&row.agent].open_turn, Some(turn));
    assert!(resumed.ctx.host.spawned().is_empty());
    let (_, attempt) = resumed.sup.world().instructions.unresolved_launch_attempts(work.agent)
        .find(|(_, attempt)| matches!(attempt.owner, dudes_proto::InstructionObservationOwner::Managed { turn: actual, .. } if actual == turn)).unwrap();
    assert!(attempt.boundary_settled.is_none());
    assert!(matches!(
        attempt.finished.as_ref().map(|finish| &finish.settlement),
        Some(dudes_proto::InstructionSettlement::Unknown {
            reason: dudes_proto::InstructionUnknownReason::UnrecordedLaunchBoundary,
            ..
        })
    ));
    let inspected = resumed
        .recovery_inspect(work.agent, operator_admission())
        .unwrap();
    let Answered::RecoveryInspected(RecoveryInspectResult::Available {
        expected, choices, ..
    }) = inspected
    else {
        panic!("{inspected:?}")
    };
    assert_eq!(choices, RecoveryAllowedChoices::RepeatIssuedFresh);
    assert_eq!(expected.expected_predecessor, None);
    let new_episode = expected.episode.unwrap();
    assert!(fixture.entries().iter().any(|(lsn, entry)| matches!(&entry.record, Record::RecoveryRequired(required) if *lsn == new_episode.origin && required.binding.work.turn == turn && required.predecessor == Some(receipt.claim) && required.reason == dudes_proto::recovery::RecoveryReason::MissingSpawn)));
    assert!(
        matches!(resumed.ctx.recovery_read(request), RecoveryReceiptResult::Found { receipt: ref current } if current.claim == receipt.claim && matches!(current.status, RecoveryReceiptState::Superseded { successor, .. } if successor == new_episode))
    );
    assert!(!fixture.records().iter().any(|record| matches!(record, Record::TurnSpawned { turn: actual, .. } | Record::TurnEnded { turn: actual, .. } if *actual == turn)));
    let floor = resumed.floor.as_ref().unwrap();
    let sessions = floor
        .carry(crate::baseline::CheckRuns::default())
        .sessions()
        .clone();
    let identity = sign_in(&sessions, &floor.addr().to_string());
    let repeated = RecoverySubmitParams {
        request_id: Hex32::new([126; 32]),
        expected: *expected,
        choice: WireChoice::RepeatIssuedFresh,
    };
    let new_host = resumed.ctx.host.clone();
    let mut rebooted = common::start(resumed);
    let accepted = rebooted.handle.submit_floor_recovery(identity, &repeated);
    assert!(
        matches!(accepted, RecoverySubmitResult::Accepted { .. }),
        "{accepted:?}"
    );
    let repeated_receipt = await_consumed(fixture, &rebooted.handle, repeated.request_id);
    assert!(matches!(
        repeated_receipt.status,
        RecoveryReceiptState::Consumed { .. }
    ));
    assert_eq!(new_host.spawned().len(), 1);
    assert_eq!(fixture.records().iter().filter(|record| matches!(record, Record::TurnIssued { turn: actual, .. } if *actual == turn)).count(), 1);
    rebooted.stop(crate::ExitCause::Ended);
}

#[test]
fn an_actual_spawn_result_is_recorded_after_later_original_authority_loss() {
    let mut owned = browser_fixture();
    let sessions = owned.sessions.clone();
    let (lost, observed) = std::sync::mpsc::channel();
    owned.daemon.ctx.before_recovery_input = Some(Box::new(move |input| {
        if matches!(input, dudes_supervisor::Input::Spawned { .. }) {
            invalidate_without_a_retained_registry_guard(&sessions);
            lost.send(()).unwrap();
        }
    }));
    let host = owned.daemon.ctx.host.clone();
    let mut running = common::start(owned.daemon);
    let submitted = running
        .handle
        .submit_floor_recovery(owned.identity.clone(), &owned.intent);
    assert!(matches!(submitted, RecoverySubmitResult::Accepted { .. }));
    observed
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let receipt = await_consumed(&owned.fixture, &running.handle, owned.intent.request_id);
    assert!(matches!(
        receipt.status,
        RecoveryReceiptState::Consumed { .. }
    ));
    assert_eq!(host.spawned().len(), 1);
    let origin = RecoveryAttribution::BrowserSession {
        session_digest: owned.identity.digest(),
    };
    assert_eq!(
        owned.sessions.recovery_authority().observe_original(
            &origin,
            crate::floor_session::RecoveryGrants {
                recovery: true,
                pause_resume: true
            }
        ),
        dudes_supervisor::RecoveryAvailability::EstablishedUnavailable
    );
    assert!(
        !owned
            .fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::RecoveryInvalidated(_)))
    );
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn the_separate_start_scope_refreshes_after_queued_inputs_and_work_refresh() {
    for established in [false, true] {
        let mut owned = browser_fixture();
        wait_for_human(&mut owned.daemon, owned.intent.expected.work.agent);
        let submitted = owned
            .daemon
            .recovery_submit_browser(owned.identity.clone(), &owned.intent);
        assert!(
            matches!(submitted, RecoverySubmitResult::Accepted { ref receipt } if receipt.status == RecoveryReceiptState::Pending)
        );
        let before = owned.fixture.entries().len();
        let sessions = owned.sessions.clone();
        let mut refreshes = 0;
        owned
            .daemon
            .pump_inputs_with_work_refresh(
                &mut PumpReport::Discard,
                PumpOrigin::Ordinary,
                std::collections::VecDeque::from([dudes_supervisor::Input::Tick]),
                &[],
                |sup, operation| {
                    sup.handle(dudes_supervisor::Input::Started, operation)
                        .map_err(anyhow::Error::new)
                },
                |daemon, _| {
                    daemon.refresh_work()?;
                    refreshes += 1;
                    if refreshes == 2 {
                        if established {
                            invalidate_without_a_retained_registry_guard(&sessions);
                        } else {
                            daemon.ctx.recovery_authority = None;
                        }
                    }
                    Ok(())
                },
            )
            .unwrap();
        assert!(refreshes >= 2);
        let frames = command_frames(&owned.fixture, before);
        assert_eq!(
            frames
                .iter()
                .filter(|kind| **kind == RecordKind::RecoveryInvalidated)
                .count(),
            usize::from(established)
        );
        assert!(!frames.iter().any(|kind| matches!(
            kind,
            RecordKind::LoopDecided
                | RecordKind::RecoveryRequired
                | RecordKind::Resume
                | RecordKind::RecoveryAuthorized
                | RecordKind::TurnIssued
                | RecordKind::TurnSpawned
        )));
        assert!(owned.daemon.ctx.host.spawned().is_empty());
        let receipt = owned.daemon.ctx.recovery_read(owned.intent.request_id);
        assert!(
            matches!(receipt, RecoveryReceiptResult::Found { ref receipt } if matches!((&receipt.status, established), (RecoveryReceiptState::Pending, false) | (RecoveryReceiptState::Invalidated { .. }, true)))
        );
        let mut running = common::start(owned.daemon);
        running.stop(crate::ExitCause::Ended);
    }
}

#[test]
fn confirmed_original_registry_reaches_guarded_writes_and_the_actual_writer_pump() {
    let BrowserFixture {
        fixture,
        mut daemon,
        sessions,
        identity,
        intent,
        ..
    } = browser_fixture();
    let host = daemon.ctx.host.clone();
    let authority = daemon.ctx.recovery_authority.clone().unwrap();
    let (release, delayed) = std::sync::mpsc::channel();
    daemon.ctx.before_recovery_append = Some(Box::new(move |record| {
        if matches!(record, Record::TurnIssued { .. }) {
            delayed
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("the test releases the actual delayed issue");
        }
    }));
    let mut running = common::start(daemon);
    let submitted = running
        .handle
        .submit_floor_recovery(identity.clone(), &intent);
    let RecoverySubmitResult::Accepted { receipt } = submitted else {
        panic!("submission={submitted:?}")
    };
    assert!(matches!(receipt.status, RecoveryReceiptState::Pending));
    assert!(host.spawned().is_empty());
    release.send(()).unwrap();
    let receipt = await_consumed(&fixture, &running.handle, intent.request_id);
    let entries = fixture.entries();
    assert!(
        matches!(receipt.status, RecoveryReceiptState::Consumed { .. }),
        "receipt={receipt:?}; records={:?}; decisions={:?}; spawns={}",
        entries
            .iter()
            .map(|(_, entry)| entry.record.kind())
            .collect::<Vec<_>>(),
        entries
            .iter()
            .filter_map(|(_, entry)| match &entry.record {
                Record::LoopDecided {
                    decision,
                    gate_trace,
                    ..
                } => Some((decision, gate_trace)),
                _ => None,
            })
            .collect::<Vec<_>>(),
        host.spawned().len()
    );
    assert_eq!(host.spawned().len(), 1);
    let authorization = entries
        .iter()
        .find_map(|(_, entry)| match &entry.record {
            Record::RecoveryAuthorized(authorization) => Some(authorization),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        authorization.attribution,
        RecoveryAttribution::BrowserSession {
            session_digest: identity.digest()
        }
    );
    assert!(entries.iter().any(|(_, entry)| matches!(&entry.record, Record::Resume { browser: Some(browser), .. }
        if browser.request_id == intent.request_id && browser.request_digest == authorization.request_digest && browser.session_digest == identity.digest())));
    assert_eq!(
        authority.observe_original(
            &authorization.attribution,
            crate::floor_session::RecoveryGrants {
                recovery: true,
                pause_resume: true
            }
        ),
        dudes_supervisor::RecoveryAvailability::Live
    );
    running.stop(crate::ExitCause::Ended);
    assert_eq!(
        authority.observe_original(
            &authorization.attribution,
            crate::floor_session::RecoveryGrants {
                recovery: true,
                pause_resume: true
            }
        ),
        dudes_supervisor::RecoveryAvailability::EstablishedUnavailable
    );
    assert!(sessions.issue().is_err());
}

fn await_consumed(
    fixture: &common::Fixture,
    handle: &crate::Handle,
    request: Hex32,
) -> RecoveryReceiptView {
    let turn = fixture.await_record("the exact reserved recovery turn", |record| match record {
        Record::RecoveryAuthorized(authorization) if authorization.request_id == request => {
            match authorization.plan {
                RecoveryPlan::New { turn, .. } => Some(turn),
                RecoveryPlan::RepeatIssued { ref issue, .. } => Some(issue.issued.work.turn),
            }
        }
        _ => None,
    });
    fixture.await_record("the actual continued recovery Spawn", |record| {
        matches!(record, Record::TurnSpawned { turn: actual, .. } if *actual == turn).then_some(())
    });
    let result = dudes_rpc::methods::recovery::RecoveryControl::recovery_receipt(
        handle,
        dudes_proto::rpc::recovery::RecoveryReceiptParams {
            request_id: request,
        },
        operator_admission(),
    )
    .unwrap();
    let RecoveryReceiptResult::Found { receipt } = result else {
        panic!("receipt={result:?}")
    };
    *receipt
}

#[test]
fn checked_subjects_follow_authorization_through_the_writer_continuation() {
    let mut owned = browser_fixture();
    let submitted = owned
        .daemon
        .recovery_submit_browser(owned.identity.clone(), &owned.intent);
    let work = owned.intent.expected.work;
    let subjects = owned
        .daemon
        .ctx
        .detached_recovery_subjects(owned.daemon.sup.world(), &[work]);
    let row = &owned.daemon.sup.world().agents[&work.agent];
    assert!(
        matches!(&submitted, RecoverySubmitResult::Accepted { receipt } if matches!(receipt.status, RecoveryReceiptState::Pending)),
        "submitted={submitted:?}; subjects={subjects:?}; control={:?}; hold={:?}; question={:?}; provider_present={}",
        row.control_lsn,
        row.automatic_hold,
        row.open_question,
        owned
            .daemon
            .sup
            .adapters()
            .get(dudes_types::Provider::Claude)
            .is_some()
    );
    assert!(owned.daemon.ctx.host.spawned().is_empty());
    let host = owned.daemon.ctx.host.clone();
    let mut running = common::start(owned.daemon);
    let receipt = await_consumed(&owned.fixture, &running.handle, owned.intent.request_id);
    assert!(matches!(
        receipt.status,
        RecoveryReceiptState::Consumed { .. }
    ));
    assert_eq!(host.spawned().len(), 1);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn disabled_grants_and_a_foreign_identity_refuse_before_any_command_append() {
    for denied in 0..3 {
        let mut owned = browser_fixture();
        let foreign = browser_fixture();
        let before = owned.fixture.entries();
        let identity = match denied {
            0 => {
                let live = owned.daemon.ctx.switches.load();
                owned
                    .daemon
                    .ctx
                    .switches
                    .store(crate::floor_switches::Switches {
                        recovery: false,
                        ..live
                    });
                owned.identity.clone()
            }
            1 => {
                let live = owned.daemon.ctx.switches.load();
                owned
                    .daemon
                    .ctx
                    .switches
                    .store(crate::floor_switches::Switches {
                        control: FloorControl::Off,
                        ..live
                    });
                owned.identity.clone()
            }
            _ => foreign.identity.clone(),
        };
        let submitted = owned
            .daemon
            .recovery_submit_browser(identity, &owned.intent);
        assert_eq!(
            submitted,
            RecoverySubmitResult::Refused {
                reason: RecoverySubmitRefusal::AuthorityUnavailable,
                prefix: RecoveryKnownPrefix {
                    required: None,
                    resume: None
                },
            }
        );
        assert_eq!(owned.fixture.entries(), before);
        assert!(owned.daemon.ctx.host.spawned().is_empty());
        assert!(!owned.daemon.ctx.handle.journal_uncertain());
        let mut running = common::start(owned.daemon);
        running.stop(crate::ExitCause::Ended);
        let mut other = common::start(foreign.daemon);
        other.stop(crate::ExitCause::Ended);
    }
}

#[test]
fn one_runtime_scope_joins_each_claim_to_its_own_original_registry_row() {
    let mut owned = browser_fixture();
    wait_for_human(&mut owned.daemon, owned.intent.expected.work.agent);
    assert!(matches!(
        owned
            .daemon
            .recovery_submit_browser(owned.identity.clone(), &owned.intent),
        RecoverySubmitResult::Accepted { .. }
    ));
    owned.clock.advance(12 * 60 * 60_000);
    let second = failed_source_at(&mut owned.daemon, &owned.fixture, 916);
    wait_for_human(&mut owned.daemon, second);
    let second_intent = inspected_intent(&mut owned.daemon, second, 124);
    let second_identity = sign_in(
        &owned.sessions,
        &owned.daemon.floor_addr().unwrap().to_string(),
    );
    assert!(matches!(
        owned
            .daemon
            .recovery_submit_browser(second_identity, &second_intent),
        RecoverySubmitResult::Accepted { .. }
    ));
    owned.clock.advance(12 * 60 * 60_000);
    let before = owned.fixture.entries().len();
    owned
        .daemon
        .pump_with_report(dudes_supervisor::Input::Started, &mut PumpReport::Discard)
        .unwrap();
    let entries = owned.fixture.entries();
    let invalidated = entries
        .iter()
        .skip(before)
        .filter_map(|(_, entry)| match &entry.record {
            Record::RecoveryInvalidated(record) => Some(record.claim),
            _ => None,
        })
        .collect::<Vec<_>>();
    let first = match owned.daemon.ctx.recovery_read(owned.intent.request_id) {
        RecoveryReceiptResult::Found { receipt } => *receipt,
        other => panic!("{other:?}"),
    };
    assert_eq!(invalidated, vec![first.claim]);
    assert!(matches!(
        first.status,
        RecoveryReceiptState::Invalidated { .. }
    ));
    assert!(
        matches!(owned.daemon.ctx.recovery_read(second_intent.request_id), RecoveryReceiptResult::Found { ref receipt } if receipt.status == RecoveryReceiptState::Pending)
    );
    assert!(entries.iter().skip(before).any(|(_, entry)| matches!(entry.record, Record::LoopDecided { agent, decision: dudes_proto::LoopDecision::Wait { .. }, .. } if agent == second)));
    assert!(owned.daemon.ctx.host.spawned().is_empty());
    let mut running = common::start(owned.daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn a_real_replay_admits_an_explicit_repeat_of_a_legacy_unspawned_issue() {
    let fixture = common::Fixture::new();
    let settings = floor_settings(&fixture, 0);
    let clock = Arc::new(common::TestClock::ticking(1_787_394_600_000, 0));
    let mut daemon = boot_at(&settings, &clock);
    let agent = issued_source_at(&mut daemon, &fixture, 917);
    let turn = TurnId::from_parts(917, 3);
    let original = daemon.sup.world().turns[&turn].clone();
    assert!(
        daemon
            .sup
            .world()
            .instructions
            .current_for_turn(turn)
            .is_none()
    );
    drop(daemon);
    let daemon = boot_at(&settings, &clock);
    let mut owned = signed_fixture(fixture, daemon, agent, clock);
    owned.intent.choice = WireChoice::RepeatIssuedFresh;
    assert_eq!(owned.intent.expected.work.turn, turn);
    assert_eq!(owned.intent.expected.issue_lsn, original.issue_lsn);
    let host = owned.daemon.ctx.host.clone();
    let mut running = common::start(owned.daemon);
    let submitted = running
        .handle
        .submit_floor_recovery(owned.identity, &owned.intent);
    assert!(
        matches!(submitted, RecoverySubmitResult::Accepted { .. }),
        "{submitted:?}"
    );
    let receipt = await_consumed(&owned.fixture, &running.handle, owned.intent.request_id);
    assert!(matches!(
        receipt.status,
        RecoveryReceiptState::Consumed { .. }
    ));
    assert_eq!(host.spawned().len(), 1);
    let entries = owned.fixture.entries();
    assert_eq!(entries.iter().filter(|(_, entry)| matches!(entry.record, Record::TurnIssued { turn: actual, .. } if actual == turn)).count(), 1);
    let Record::TurnIssued { task, answers, .. } = &entries
        .iter()
        .find(|(lsn, _)| *lsn == original.issue_lsn)
        .unwrap()
        .1
        .record
    else {
        unreachable!()
    };
    assert_eq!((task, answers), (&original.task, &original.answers));
    assert!(entries.iter().any(|(_, entry)| matches!(&entry.record, Record::RecoveryAuthorized(authorization) if matches!(&authorization.plan, RecoveryPlan::RepeatIssued { issue, .. } if issue.issued.work.turn == turn && issue.lsn == original.issue_lsn))));
    running.stop(crate::ExitCause::Ended);
}

mod routes;
mod ticket_context;
