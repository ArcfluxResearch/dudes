use super::*;
use crate::ports::Ask;
use dudes_proto::recovery::RecoveryOrigin;
use dudes_proto::rpc::recovery::RecoveryInspectUnavailable;
use dudes_proto::{AutomaticHold, ExitStatus, Spend, TurnOutcome};
use dudes_supervisor::Input;

fn observed_failed_source(daemon: &mut TestDaemon, fixture: &common::Fixture) -> AgentId {
    observed_failure_source(
        daemon,
        fixture,
        &TurnOutcome::Failed {
            reason: "the owned provider reported failure".into(),
        },
    )
}

fn observed_no_result_source(daemon: &mut TestDaemon, fixture: &common::Fixture) -> AgentId {
    observed_failure_source(daemon, fixture, &TurnOutcome::NoResult)
}

fn observed_failure_source(
    daemon: &mut TestDaemon,
    fixture: &common::Fixture,
    outcome: &TurnOutcome,
) -> AgentId {
    let agent = AgentId::from_parts(925, 1);
    let goal = GoalId::from_parts(925, 2);
    let first = TurnId::from_parts(925, 3);
    let latest = TurnId::from_parts(925, 4);
    source_hire(daemon, fixture, agent);
    source_goal(daemon, agent, goal);
    for turn in [first, latest] {
        observed_process_turn(daemon, agent, goal, turn, outcome.clone());
    }
    append_history(
        daemon,
        Record::AgentHeld {
            agent,
            provenance: Provenance::Observed,
            cause: AutomaticHold::RepeatedFastFailure { first, latest },
        },
    );
    agent
}

fn observed_process_turn(
    daemon: &mut TestDaemon,
    agent: AgentId,
    goal: GoalId,
    turn: TurnId,
    outcome: TurnOutcome,
) {
    let decision = daemon.ctx.writer.next_lsn().unwrap();
    append_history(
        daemon,
        Record::LoopDecided {
            agent,
            input_digest: "34".repeat(32),
            decision: dudes_proto::LoopDecision::Continue {
                task: "finish the owned fixture".into(),
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
            task: "finish the owned fixture".into(),
            session: None,
            mode: Mode::Managed,
            model_selection: Some(ModelSelection::provider_default()),
            lsn_of_decision: decision,
            answers: Vec::new(),
        },
    );
    let mut spawned = samples::record(RecordKind::TurnSpawned);
    let Record::TurnSpawned {
        agent: owner,
        turn: issued,
        pid,
        session,
        ..
    } = &mut spawned
    else {
        unreachable!()
    };
    *owner = agent;
    *issued = turn;
    *pid = 9_000 + daemon.sup.world().goals[&goal].turns_on_goal;
    *session = None;
    append_history(daemon, spawned);
    append_history(
        daemon,
        Record::TurnEnded {
            agent,
            turn,
            mode: Mode::Managed,
            provenance: Provenance::Observed,
            exit: ExitStatus {
                code: Some(1),
                signal: None,
            },
            usage: None,
            model_usage: BTreeMap::new(),
            last_message: None,
            session: None,
            outcome: Some(outcome),
            elapsed_ms: Some(1),
            final_reply: None,
            session_usage: None,
        },
    );
}

fn invalidate_authorization(owned: &mut BrowserFixture) -> RecoveryReceiptView {
    assert!(
        owned.daemon.sup.world().agents[&owned.intent.expected.work.agent]
            .automatic_hold
            .is_some()
    );
    let sessions = owned.sessions.clone();
    let original = owned.intent.request_id;
    owned.daemon.ctx.after_recovery_fsync = Some(Box::new(move |record| {
        if matches!(record, Record::RecoveryAuthorized(authorization) if authorization.request_id == original)
        {
            invalidate_without_a_retained_registry_guard(&sessions);
        }
    }));
    let submitted = owned
        .daemon
        .recovery_submit_browser(owned.identity.clone(), &owned.intent);
    owned.daemon.ctx.after_recovery_fsync = None;
    let RecoverySubmitResult::Accepted { receipt } = submitted else {
        panic!("the browser consent must have its durable receipt: {submitted:?}");
    };
    assert!(matches!(
        receipt.status,
        RecoveryReceiptState::Invalidated {
            reason: RecoveryReceiptInvalidation::AuthorityUnavailable,
            ..
        }
    ));
    assert_eq!(receipt.request_id, original);
    assert_eq!(
        receipt.attempt,
        RecoveryAttemptView {
            spawn: None,
            confirmation: None,
            end: None,
        }
    );
    let agent = &owned.daemon.sup.world().agents[&owned.intent.expected.work.agent];
    assert!(!agent.paused && !agent.halted && agent.automatic_hold.is_none());
    assert_eq!(agent.open_turn, None);
    assert_eq!(agent.goal, Some(owned.intent.expected.work.goal));
    assert_eq!(
        owned
            .daemon
            .sup
            .world()
            .completed_goal_lsn(owned.intent.expected.work.goal),
        None
    );
    assert!(owned.daemon.ctx.host.spawned().is_empty());
    *receipt
}

fn accounting(owned: &BrowserFixture) -> (u32, u32, Spend) {
    let goal = owned.intent.expected.work.goal;
    (
        owned.daemon.sup.world().goals[&goal].turns_on_goal,
        owned
            .daemon
            .ctx
            .projections
            .goal(goal)
            .unwrap()
            .unwrap()
            .turns_on_goal,
        owned.daemon.ctx.projections.spend_by_goal(goal).unwrap(),
    )
}

fn assert_old_plan_unissued(fixture: &common::Fixture, old_request: Hex32) {
    let turn = authorized_turn(fixture, old_request);
    assert!(!fixture.records().iter().any(|record| matches!(record,
        Record::TurnIssued { turn: issued, .. }
        | Record::TurnSpawned { turn: issued, .. }
        | Record::TurnEnded { turn: issued, .. } if *issued == turn)));
}

fn assert_old_receipt(owned: &mut BrowserFixture, old: &RecoveryReceiptView) {
    let before = owned.fixture.entries();
    assert_eq!(
        owned.daemon.ctx.recovery_read(old.request_id),
        RecoveryReceiptResult::Found {
            receipt: Box::new(*old),
        }
    );
    assert_eq!(owned.fixture.entries(), before);
    assert_old_plan_unissued(&owned.fixture, old.request_id);
}

fn ordinary_controls_keep_the_claim_closed(owned: &mut BrowserFixture, old: &RecoveryReceiptView) {
    let before = owned.fixture.entries().len();
    let accounted = accounting(owned);
    owned
        .daemon
        .ask(Ask::Resume(owned.intent.expected.work.agent))
        .unwrap();
    owned.daemon.pump(Input::Tick).unwrap();
    assert_eq!(accounting(owned), accounted);
    assert_eq!(
        command_frames(&owned.fixture, before),
        vec![RecordKind::Resume]
    );
    assert!(owned.daemon.ctx.host.spawned().is_empty());
    assert_not_recoverable(owned);
    assert_old_receipt(owned, old);
}

fn assert_not_recoverable(owned: &mut BrowserFixture) {
    assert!(matches!(
        owned
            .daemon
            .recovery_inspect(owned.intent.expected.work.agent, operator_admission())
            .unwrap(),
        Answered::RecoveryInspected(RecoveryInspectResult::Unavailable {
            reason: RecoveryInspectUnavailable::NotRecoverable,
        })
    ));
}

fn submit_local(owned: &mut BrowserFixture, intent: &RecoverySubmitParams) -> RecoveryReceiptView {
    let admission = operator_admission();
    let command = crate::ports::recovery_command(
        intent,
        RecoveryAttribution::LocalOperator {
            pid: admission.pid(),
            uid: admission.uid(),
        },
    )
    .unwrap();
    let submitted = owned.daemon.recovery_submit(command, admission);
    let Answered::RecoverySubmitted(RecoverySubmitResult::Accepted { receipt }) = submitted else {
        panic!("the later Pause must permit the new explicit local consent: {submitted:?}");
    };
    *receipt
}

fn prove_later_pause_progress(source: fn(&mut TestDaemon, &common::Fixture) -> AgentId) {
    let mut owned = browser_fixture_from_source(0, source);
    let work = owned.intent.expected.work;
    let old = invalidate_authorization(&mut owned);
    ordinary_controls_keep_the_claim_closed(&mut owned, &old);
    let RecoveryReceiptState::Invalidated {
        invalidation_lsn, ..
    } = old.status
    else {
        unreachable!()
    };
    let before = owned.fixture.entries().len();
    let accounted = accounting(&owned);
    owned.daemon.ask(Ask::Pause(work.agent)).unwrap();
    let pause = owned.daemon.sup.world().agents[&work.agent]
        .control_lsn
        .unwrap();
    assert!(pause > invalidation_lsn);
    let next = inspected_intent(&mut owned.daemon, work.agent, 142);
    assert_eq!(next.expected.work, work);
    assert_eq!(next.expected.expected_predecessor, Some(old.claim));
    assert_eq!(next.expected.source_lsn, pause);
    assert_eq!(accounting(&owned), accounted);
    assert!(owned.daemon.ctx.host.spawned().is_empty());
    let accepted = submit_local(&mut owned, &next);
    assert_ne!(accepted.claim.episode, old.claim.episode);
    assert_eq!(accepted.claim.episode.work, work);
    assert!(accepted.claim.authorization_lsn > invalidation_lsn);
    assert!(accepted.claim.authorization_lsn > pause);
    let turn = authorized_turn(&owned.fixture, next.request_id);
    assert_ne!(turn, authorized_turn(&owned.fixture, old.request_id));
    assert_ne!(turn, work.turn);
    assert_old_receipt(&mut owned, &old);
    assert!(owned.fixture.records().iter().any(|record| matches!(record,
        Record::RecoveryRequired(required)
            if required.predecessor == Some(old.claim)
                && required.binding.work == work
                && required.origin == RecoveryOrigin::Existing { lsn: pause })));
    let host = owned.daemon.ctx.host.clone();
    let mut running = common::start(owned.daemon);
    let receipt = await_consumed(&owned.fixture, &running.handle, next.request_id);
    assert!(
        matches!(receipt.status, RecoveryReceiptState::Consumed { turn: issued, .. } if issued == turn)
    );
    assert_eq!(read_receipt(&running.handle, old.request_id), old);
    assert_old_plan_unissued(&owned.fixture, old.request_id);
    assert_eq!(host.spawned().len(), 1);
    let frames = command_frames(&owned.fixture, before);
    for kind in [
        RecordKind::RecoveryAuthorized,
        RecordKind::TurnIssued,
        RecordKind::TurnSpawned,
    ] {
        assert_eq!(
            frames.iter().filter(|frame| **frame == kind).count(),
            1,
            "{kind:?}"
        );
    }
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn an_observed_failure_can_get_new_consent_after_an_invalidated_claim() {
    prove_later_pause_progress(observed_failed_source);
}

#[test]
fn an_observed_no_result_can_get_new_consent_after_an_invalidated_claim() {
    prove_later_pause_progress(observed_no_result_source);
}

#[test]
fn a_later_pause_does_not_reclassify_a_preflight_launch_failure() {
    let mut owned = browser_fixture();
    let old = invalidate_authorization(&mut owned);
    ordinary_controls_keep_the_claim_closed(&mut owned, &old);
    let before = owned.fixture.entries().len();
    let accounted = accounting(&owned);
    owned
        .daemon
        .ask(Ask::Pause(owned.intent.expected.work.agent))
        .unwrap();
    owned.daemon.pump(Input::Tick).unwrap();
    assert_not_recoverable(&mut owned);
    assert_eq!(accounting(&owned), accounted);
    assert_eq!(
        command_frames(&owned.fixture, before),
        vec![RecordKind::Pause]
    );
    assert!(owned.daemon.ctx.host.spawned().is_empty());
    assert_old_receipt(&mut owned, &old);
    let mut running = common::start(owned.daemon);
    running.stop(crate::ExitCause::Ended);
}
