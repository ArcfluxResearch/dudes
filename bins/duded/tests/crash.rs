//! Cleanup uncertainty is run-local, so every daemon exit leaves the same
//! issued-only journal prefix for replay.
//!
//! # What "a crash between the append and the launch" is, on the disk
//!
//! `Writer::append` returns only after the fsync, and the machine parks the `Spawn` behind that
//! acknowledgement. So the state a crash in that window leaves behind is exactly: a `TurnIssued`
//! durable, and no `TurnSpawned`.
//!
//! # How the two exit arms construct that state
//!
//! The first run's host reports `CleanupUnconfirmed`. The fake models the runtime result only;
//! native `dudes-host` tests own the retained-process half. The runtime queues no input, leaving a
//! durable `TurnIssued` with no `TurnSpawned` or `TurnEnded`. One arm stops normally. The other
//! starts this test binary as a helper daemon, waits until a real retained host row proves the
//! uncertain launch is in flight, and kills the whole helper process without its exit path. Each
//! second run replays the issued-only prefix.
//!
//! # The control
//!
//! The same fixture without the crash also yields exactly one child:
//! [`the_same_fixture_without_the_refusal_yields_exactly_one_child`].
//! Without it, a daemon that launched nothing at all would pass the arm above.
use std::ffi::OsString;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::Record;
use dudes_providers::launch::{EnvVar, LaunchSpec};

mod common;

use common::{Fixture, ScriptedRunner, TestClock, UnconfirmedHost, seams};

const ABRUPT_ROLE: &str = "DUDES_ABRUPT_UNCERTAIN_ROLE";
const ABRUPT_STATE: &str = "DUDES_ABRUPT_UNCERTAIN_STATE";
const ABRUPT_RUNTIME: &str = "DUDES_ABRUPT_UNCERTAIN_RUNTIME";
const ABRUPT_READY: &str = "DUDES_ABRUPT_UNCERTAIN_READY";
const ABRUPT_READY_PAYLOAD: &str = "spawns=1 owned=1";

fn helper_env(role: &str) -> Vec<EnvVar> {
    let mut env: Vec<EnvVar> = [
        "PATH",
        "HOME",
        "TMPDIR",
        "SystemRoot",
        "windir",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "SystemDrive",
    ]
    .into_iter()
    .filter_map(|name| std::env::var_os(name).map(|value| EnvVar::new(name, value)))
    .collect();
    env.push(EnvVar::new(ABRUPT_ROLE, role));
    env
}

fn abrupt_agent_spec(cwd: &Path) -> LaunchSpec {
    LaunchSpec {
        argv: vec![
            OsString::from(std::env::current_exe().expect("current test executable")),
            "--exact".into(),
            "abrupt_uncertain_daemon_helper".into(),
            "--quiet".into(),
        ],
        env: helper_env("agent"),
        cwd: cwd.to_path_buf(),
        stdin: dudes_providers::launch::LaunchInput::WriteThenClose(vec![b'x'; 1 << 20]),
    }
}

/// The outer helper runs a complete daemon; its inner helper exits to produce the real EPIPE.
#[test]
fn abrupt_uncertain_daemon_helper() {
    let Ok(role) = std::env::var(ABRUPT_ROLE) else {
        return;
    };
    if role == "agent" {
        std::process::exit(0);
    }
    assert_eq!(role, "daemon");

    let state = std::path::PathBuf::from(std::env::var_os(ABRUPT_STATE).expect("state path"));
    let runtime = std::path::PathBuf::from(std::env::var_os(ABRUPT_RUNTIME).expect("runtime path"));
    let ready = std::path::PathBuf::from(std::env::var_os(ABRUPT_READY).expect("ready path"));
    let host = common::RetainedFailureHost::new(abrupt_agent_spec(&state));
    let settings = common::settings_at(&state, &runtime);
    let daemon = boot(
        &settings,
        seams(
            &settings,
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::ticking(1_787_394_600_000, 1)),
            Witness::default(),
        ),
    )
    .expect("helper daemon boots");
    let running = common::start(daemon);
    let agent = common::hire(&running.handle, &state);
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::true_program()]),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while host.spawns() != 1 || host.owned() != 1 {
        assert!(
            Instant::now() < deadline,
            "the helper daemon did not retain exactly one uncertain launch"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(&ready, ABRUPT_READY_PAYLOAD).expect("publish abrupt readiness");
    loop {
        std::thread::park();
    }
}

#[allow(
    clippy::disallowed_types,
    reason = "the test harness starts the helper daemon process that it kills abruptly"
)]
fn abrupt_daemon_command(fixture: &Fixture, ready: &Path) -> std::process::Command {
    let mut command =
        std::process::Command::new(std::env::current_exe().expect("current test executable"));
    command
        .args(["--exact", "abrupt_uncertain_daemon_helper", "--quiet"])
        .env_clear();
    for var in helper_env("daemon") {
        command.env(var.name(), var.expose_value());
    }
    command
        .env(ABRUPT_STATE, fixture.state.path())
        .env(ABRUPT_RUNTIME, fixture.runtime.path())
        .env(ABRUPT_READY, ready)
        .current_dir(fixture.state.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    command
}

fn complete_abrupt_readiness(ready: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(ready) {
        Ok(payload) => Ok((payload == ABRUPT_READY_PAYLOAD).then_some(payload)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn await_abrupt_ready_core(
    deadline: Instant,
    mut read: impl FnMut() -> std::io::Result<Option<String>>,
    mut child_status: impl FnMut() -> std::io::Result<Option<std::process::ExitStatus>>,
    mut sleep: impl FnMut(Duration),
) -> String {
    loop {
        if let Some(payload) = read().expect("read readiness") {
            return payload;
        }
        if let Some(status) = child_status().expect("poll helper daemon") {
            panic!("the helper daemon exited before readiness: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "the helper daemon did not publish readiness"
        );
        sleep(Duration::from_millis(10));
    }
}

fn await_abrupt_ready(child: &mut Child, ready: &Path) -> String {
    await_abrupt_ready_core(
        Instant::now() + Duration::from_secs(20),
        || complete_abrupt_readiness(ready),
        || child.try_wait(),
        std::thread::sleep,
    )
}

fn abrupt_exit_arm() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let ready = fixture.state.path().join("abrupt-uncertain.ready");
    let mut child = abrupt_daemon_command(&fixture, &ready)
        .spawn()
        .expect("start helper daemon");
    let readiness = await_abrupt_ready(&mut child, &ready);
    assert_eq!(
        readiness, ABRUPT_READY_PAYLOAD,
        "the abrupt exit must intersect one real retained uncertain launch"
    );

    child.kill().expect("abruptly terminate helper daemon");
    let status = child.wait().expect("reap helper daemon");
    assert!(!status.success(), "the abrupt helper exit ran cleanly");

    let prefix = fixture.records();
    assert_eq!(
        count(&prefix, |record| matches!(
            record,
            Record::TurnIssued { .. }
        )),
        1
    );
    assert_eq!(
        count(&prefix, |record| matches!(
            record,
            Record::TurnSpawned { .. }
        )),
        0
    );
    assert_eq!(
        count(&prefix, |record| matches!(record, Record::TurnEnded { .. })),
        0
    );

    let restarted = UnconfirmedHost::new(false);
    let daemon = boot(
        &fixture.settings(),
        seams(
            &settings,
            restarted.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::ticking(1_787_394_700_000, 1)),
            Witness::default(),
        ),
    )
    .expect("restart after abrupt uncertain exit");
    let mut running = common::start(daemon);
    let (agent, turn) = prefix
        .iter()
        .find_map(|record| match record {
            Record::TurnIssued { agent, turn, .. } => Some((*agent, *turn)),
            _ => None,
        })
        .unwrap();
    authorize_owed_issue(&fixture, &settings, agent, turn);
    fixture.await_record("reissued TurnSpawned", |record| {
        matches!(record, Record::TurnSpawned { .. }).then_some(())
    });
    running.stop(ExitCause::Ended);

    assert_eq!(restarted.children(), 1, "replay reissued exactly once");
    let records = fixture.records();
    let turns: std::collections::BTreeSet<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::TurnIssued { turn, .. } | Record::TurnSpawned { turn, .. } => Some(*turn),
            _ => None,
        })
        .collect();
    assert_eq!(turns.len(), 1, "the abrupt replay kept the turn id");
    assert_eq!(
        count(&records, |record| matches!(
            record,
            Record::TurnSpawned { .. }
        )),
        1
    );
}

#[test]
#[allow(
    clippy::disallowed_types,
    reason = "the test starts and reaps a parked helper through the production waiter"
)]
fn abrupt_ready_waits_for_complete_payload() {
    let dir = tempfile::tempdir().expect("temporary readiness directory");
    let ready = dir.path().join("abrupt-uncertain.ready");
    let mut states = [
        ("absent", None),
        ("empty", Some("")),
        ("partial", Some("spawns=1")),
        ("wrong", Some("spawns=1 owned=0")),
        ("complete", Some(ABRUPT_READY_PAYLOAD)),
    ]
    .into_iter();
    let mut ledger = Vec::new();
    let mut polls = 0;
    let payload = await_abrupt_ready_core(
        Instant::now() + Duration::from_secs(20),
        || {
            let (name, state) = states
                .next()
                .expect("the readiness script must publish completion before it exhausts");
            match state {
                Some(state) => std::fs::write(&ready, state)?,
                None => match std::fs::remove_file(&ready) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                },
            }
            let classified = complete_abrupt_readiness(&ready)?;
            ledger.push((name, classified.clone()));
            Ok(classified)
        },
        || {
            polls += 1;
            Ok(None)
        },
        |_| {},
    );
    assert_eq!(payload, ABRUPT_READY_PAYLOAD);
    assert_eq!(
        ledger,
        vec![
            ("absent", None),
            ("empty", None),
            ("partial", None),
            ("wrong", None),
            ("complete", Some(ABRUPT_READY_PAYLOAD.to_owned())),
        ],
        "the waiter must observe every incomplete publication before completion"
    );
    assert_eq!(
        polls, 4,
        "the child is polled exactly once after each incomplete readiness read"
    );

    let ready = dir.path().join("exited-before-ready");
    let fixture = Fixture::new();
    let mut command = abrupt_daemon_command(&fixture, &ready);
    command.env_remove(ABRUPT_ROLE);
    let mut child = command.spawn().expect("start early-exit helper");
    let panic = catch_unwind(AssertUnwindSafe(|| await_abrupt_ready(&mut child, &ready)))
        .expect_err("an exited helper cannot satisfy readiness");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("non-string readiness panic");
    assert!(
        message.contains("helper daemon exited before readiness"),
        "the waiter must reject an early helper exit: {message}"
    );
}

/// Run one daemon over `fixture` with `host`, hire an agent, set it a goal, and wait for the turn
/// to be issued.
fn one_run(fixture: &Fixture, host: UnconfirmedHost, start_millis: i64) {
    let settings = fixture.settings();
    let daemon = boot(
        &fixture.settings(),
        seams(
            &settings,
            host,
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::ticking(start_millis, 1)),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::true_program()]),
    );
    fixture.await_turn();
    running.stop(ExitCause::Ended);
}

#[test]
fn cleanup_unconfirmed_is_run_local_across_any_daemon_exit() {
    let fixture = Fixture::new();
    let settings = fixture.settings();

    // Run one: the turn is journaled and the launch never happens.
    let crashed = UnconfirmedHost::new(true);
    one_run(&fixture, crashed.clone(), 1_787_394_600_000);

    let after_first = fixture.records();
    assert_eq!(
        count(&after_first, |record| matches!(
            record,
            Record::TurnIssued { .. }
        )),
        1,
        "the turn was journaled"
    );
    assert_eq!(
        count(&after_first, |record| matches!(
            record,
            Record::TurnSpawned { .. }
        )),
        0,
        "and no launch was recorded: this is the crash window's state, on the disk"
    );
    assert_eq!(
        count(&after_first, |record| matches!(
            record,
            Record::TurnEnded { .. }
        )),
        0,
        "uncertainty is not converted into a durable end"
    );
    assert_eq!(
        crashed.children(),
        0,
        "and no child exists — the 'not two' half starts from zero"
    );

    // Run two: the same state directory, replayed.
    let restarted = UnconfirmedHost::new(false);
    let daemon = boot(
        &fixture.settings(),
        seams(
            &settings,
            restarted.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::ticking(1_787_394_700_000, 1)),
            Witness::default(),
        ),
    )
    .expect("the same state directory boots again");
    let mut running = common::start(daemon);
    let (agent, turn) = after_first
        .iter()
        .find_map(|record| match record {
            Record::TurnIssued { agent, turn, .. } => Some((*agent, *turn)),
            _ => None,
        })
        .unwrap();
    authorize_owed_issue(&fixture, &settings, agent, turn);
    fixture.await_record("TurnSpawned", |record| {
        matches!(record, Record::TurnSpawned { .. }).then_some(())
    });
    running.stop(ExitCause::Ended);

    assert_eq!(
        restarted.children(),
        1,
        "exactly one child: replay re-issued the un-spawned turn once, and once only"
    );

    let records = fixture.records();
    let spawned = count(&records, |record| {
        matches!(record, Record::TurnSpawned { .. })
    });
    assert_eq!(spawned, 1, "and exactly one launch is recorded: {spawned}");

    // The turn is the *same* turn, under the same id: a re-issue, not a second turn.
    let turns: std::collections::BTreeSet<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::TurnIssued { turn, .. } | Record::TurnSpawned { turn, .. } => Some(*turn),
            _ => None,
        })
        .collect();
    assert_eq!(
        turns.len(),
        1,
        "the re-issue kept the turn's identity: {turns:?}"
    );

    abrupt_exit_arm();
}

fn finish_owned_retry(fixture: &Fixture, host: &common::SharedHost, turn: dudes_types::TurnId) {
    common::end_turn_with_native_payload(
        fixture,
        host,
        dudes_supervisor::Input::ChildExited {
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: dudes_proto::ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: Some(dudes_providers::stream::ScopedUsage::Turn(
                dudes_types::Usage::ZERO,
            )),
            model_usage: std::collections::BTreeMap::new(),
            outcome: dudes_proto::TurnOutcome::Completed,
            last_message: None,
        },
    );
    fixture.await_record("the actual retry comparison completed", |record| {
        matches!(record, Record::InstructionObservationFinished {
            owner: dudes_proto::InstructionObservationOwner::Managed { turn: owned, .. },
            settlement: dudes_proto::InstructionSettlement::Compared { .. }, ..
        } if *owned == turn)
        .then_some(())
    });
    fixture.await_record(
        "the exact retry's observed command check completed",
        |record| {
            matches!(record, Record::PredicateResult { turn: Some(owned),
            provenance: dudes_types::Provenance::Observed, .. } if *owned == turn)
            .then_some(())
        },
    );
}

#[test]
fn a_completed_owed_retry_keeps_unknown_history_without_blocking_next_work() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    one_run(&fixture, UnconfirmedHost::new(true), 1_787_394_600_000);
    let (agent, turn, task) =
        fixture.await_record("immutable issued work", |record| match record {
            Record::TurnIssued {
                agent, turn, task, ..
            } => Some((*agent, *turn, task.clone())),
            _ => None,
        });
    let (old_started, old_unknown) = fixture
        .entries()
        .into_iter()
        .find_map(|(_, entry)| {
            let Record::InstructionObservationFinished {
                owner: dudes_proto::InstructionObservationOwner::Managed { turn: owned, .. },
                baseline: dudes_proto::InstructionBaselineRef::Started { lsn },
                settlement: dudes_proto::InstructionSettlement::Unknown { .. },
                ..
            } = &entry.record
            else {
                return None;
            };
            (*owned == turn).then_some((*lsn, entry.record))
        })
        .expect("the failed attempt retains truthful uncertainty");
    let host = common::SharedHost::with_native_evidence(fixture.journal_dir());
    let seams = seams(
        &settings,
        host.clone(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::ticking(1_787_394_700_000, 1)),
        Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&seams.children));
    let mut running = common::start(boot(&fixture.settings(), seams).unwrap());
    authorize_owed_issue(&fixture, &settings, agent, turn);
    fixture.await_record("the exact owed retry actually spawned", |record| {
        matches!(record, Record::TurnSpawned { turn: owned, reissued: true, .. }
            if *owned == turn)
        .then_some(())
    });
    running.handle.ask(duded::ports::Ask::Pause(agent)).unwrap();
    finish_owned_retry(&fixture, &host, turn);
    let next_goal = common::set_goal(
        &running.handle,
        common::goal_params(agent, &[common::true_program()]),
    );
    running
        .handle
        .ask(duded::ports::Ask::Resume(agent))
        .unwrap();
    let next_issue =
        fixture.await_record(
            "the admitted fresh goal's immutable issue",
            |record| match record {
                Record::TurnIssued {
                    agent: owner,
                    goal,
                    turn: next,
                    ..
                } if *owner == agent && *goal == next_goal && *next != turn => Some(*next),
                _ => None,
            },
        );
    let next = fixture.await_record(
        "ordinary next work actually spawned",
        |record| match record {
            Record::TurnSpawned {
                agent: owner,
                turn: next,
                reissued: false,
                ..
            } if *owner == agent && *next == next_issue => Some(*next),
            _ => None,
        },
    );
    assert_ne!(next, turn);
    assert_eq!(host.spawned().len(), 2);
    let records = fixture.records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record,
        Record::TurnIssued { turn: owned, task: payload, .. }
            if *owned == turn && *payload == task))
            .count(),
        1
    );
    assert!(records.contains(&old_unknown));
    assert!(
        !records.iter().any(|record| matches!(record,
        Record::InstructionAttemptBoundarySettled { started_lsn, .. }
            if *started_lsn == old_started)),
        "the later retry's absence is never assigned to the old unknown attempt"
    );
    running.stop(ExitCause::Ended);
}

/// The control: the same fixture *without* the refusal also yields exactly one.
///
/// Without this arm a daemon that launched nothing at all would satisfy the test above.
#[test]
fn the_same_fixture_without_the_refusal_yields_exactly_one_child() {
    let fixture = Fixture::new();
    let host = UnconfirmedHost::new(false);
    one_run(&fixture, host.clone(), 1_787_394_600_000);

    assert_eq!(host.children(), 1, "one launch, one child");
    let records = fixture.records();
    assert_eq!(
        count(&records, |record| matches!(
            record,
            Record::TurnSpawned { .. }
        )),
        1,
        "and one recorded launch"
    );
}

fn recovery_call(
    settings: &duded::Settings,
    method: dudes_proto::rpc::CurrentMethod,
    params: serde_json::Value,
) -> serde_json::Value {
    let credential = dudes_proto::rpc::cred::OperatorCredential::new(
        std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .expect("the fixture daemon minted the operator credential"),
    )
    .expect("the operator credential is well formed");
    let response = dudes_rpc::Client::connect(settings.socket())
        .expect("the operator connects")
        .call_current_as_operator(method, Some(params), credential)
        .expect("one authenticated recovery request answers");
    assert!(response.error.is_none(), "{response:?}");
    response.result.expect("the published recovery result")
}

fn authorize_owed_issue(
    fixture: &Fixture,
    settings: &duded::Settings,
    agent: dudes_types::AgentId,
    turn: dudes_types::TurnId,
) {
    use dudes_proto::rpc::CurrentMethod;
    use dudes_proto::rpc::recovery::{
        RecoveryAllowedChoices, RecoveryChoice, RecoveryInspectParams, RecoveryInspectResult,
        RecoverySubmitParams, RecoverySubmitResult,
    };
    let issue_lsn = fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| {
            matches!(entry.record, Record::TurnIssued { agent: owner, turn: issued, .. }
            if owner == agent && issued == turn)
            .then_some(lsn)
        })
        .expect("the original immutable issued turn");
    fixture.await_record("the original issue requires explicit recovery", |record| {
        matches!(record, Record::RecoveryRequired(required)
            if required.binding.work.agent == agent && required.binding.work.turn == turn)
        .then_some(())
    });
    let inspected: RecoveryInspectResult = serde_json::from_value(recovery_call(
        settings,
        CurrentMethod::RecoveryInspect,
        serde_json::to_value(RecoveryInspectParams { agent }).unwrap(),
    ))
    .unwrap();
    let RecoveryInspectResult::Available {
        expected,
        reason,
        choices,
        prior_effects_unknown,
        ..
    } = inspected
    else {
        panic!("the exact owed issue offers recovery: {inspected:?}");
    };
    assert_eq!(expected.work.agent, agent);
    assert_eq!(expected.work.turn, turn);
    assert_eq!(expected.issue_lsn, issue_lsn);
    assert_eq!(reason, dudes_proto::recovery::RecoveryReason::MissingSpawn);
    assert_eq!(choices, RecoveryAllowedChoices::RepeatIssuedFresh);
    assert!(prior_effects_unknown);
    assert_eq!(
        count(&fixture.records(), |record| matches!(record,
            Record::TurnSpawned { turn: owned, .. } if *owned == turn
        )),
        0
    );
    let work = expected.work;
    let request_id = dudes_proto::record::Hex32::new([233; 32]);
    let submitted: RecoverySubmitResult = serde_json::from_value(recovery_call(
        settings,
        CurrentMethod::RecoverySubmit,
        serde_json::to_value(RecoverySubmitParams {
            request_id,
            expected: *expected,
            choice: RecoveryChoice::RepeatIssuedFresh,
        })
        .unwrap(),
    ))
    .unwrap();
    let RecoverySubmitResult::Accepted { receipt } = submitted else {
        panic!("explicit recovery must be durably accepted: {submitted:?}");
    };
    assert_eq!(receipt.request_id, request_id);
    assert_eq!(receipt.choice, RecoveryChoice::RepeatIssuedFresh);
    assert_eq!(receipt.claim.episode.work, work);
    let authorization = fixture
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::RecoveryAuthorized(authorized) if authorized.request_id == request_id => {
                Some(authorized)
            }
            _ => None,
        })
        .unwrap();
    assert!(
        matches!(authorization.plan, dudes_proto::recovery::RecoveryPlan::RepeatIssued { issue, .. }
        if issue.lsn == issue_lsn && issue.issued.work == work)
    );
    assert_eq!(
        count(&fixture.records(), |record| matches!(record,
            Record::TurnIssued { agent: owner, .. } if *owner == agent
        )),
        1
    );
}

/// How many records satisfy `f`.
fn count(records: &[Record], f: impl Fn(&Record) -> bool) -> usize {
    records.iter().filter(|record| f(record)).count()
}
