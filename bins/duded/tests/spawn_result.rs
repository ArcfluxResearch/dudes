//! Production executor mapping for the three host results.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::{Record, TurnOutcome};
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_supervisor::{Command, Input};

mod common;

const RETAINED_HELPER: &str = "DUDES_RETAINED_FAILURE_HELPER";

/// The real post-exec EPIPE child for the joined host/runtime test.
#[test]
fn retained_failure_helper() {
    if std::env::var_os(RETAINED_HELPER).is_some() {
        std::process::exit(0);
    }
}

fn retained_failure_spec(cwd: &Path) -> LaunchSpec {
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
    env.push(EnvVar::new(RETAINED_HELPER, "1"));
    LaunchSpec {
        argv: vec![
            OsString::from(std::env::current_exe().expect("current test executable")),
            "--exact".into(),
            "retained_failure_helper".into(),
            "--quiet".into(),
        ],
        env,
        cwd: cwd.to_path_buf(),
        stdin: dudes_providers::launch::LaunchInput::WriteThenClose(vec![b'x'; 1 << 20]),
    }
}

fn await_one_retained_spawn(host: &common::RetainedFailureHost) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if host.spawns() >= 1 && host.owned() >= 1 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the real failed spawn did not retain ownership"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The spawn-result test, through the production daemon executor and a real retained host child.
#[test]
fn unconfirmed_cleanup_retains_ownership_and_never_ends_the_turn() {
    let fixture = common::Fixture::new();
    let settings = fixture.settings();
    let host = common::RetainedFailureHost::new(retained_failure_spec(fixture.state.path()));
    let daemon = boot(
        &fixture.settings(),
        common::seams(
            &settings,
            host.clone(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::ticking(1_787_394_600_000, 1)),
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
    let turn = fixture.await_turn();
    await_one_retained_spawn(&host);

    running.handle.input(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    running.handle.doctor().expect("resume barrier");
    let records = fixture.records();
    // One durable issue proves no redecision. The host count is sampled after Resume and the
    // synchronous Doctor barrier, so one call proves both that no second Effect::Spawn crossed the
    // production executor and that the run-local spawning claim survived the attempted resume.
    let issued = records
        .iter()
        .filter(|record| matches!(record, Record::TurnIssued { .. }))
        .count();
    let one_host_spawn = host.spawns() == 1;
    let observations = [
        ("ownership", host.owned() == 1),
        (
            "no_input",
            records.iter().all(|record| {
                !matches!(
                    record,
                    Record::TurnEnded {
                        outcome: Some(TurnOutcome::NotLaunched { .. }),
                        ..
                    }
                )
            }),
        ),
        (
            "no_end",
            records.iter().all(
                |record| !matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn),
            ),
        ),
        ("no_redecision", issued == 1),
        ("no_second_spawn", one_host_spawn),
        ("retained_spawning", one_host_spawn),
    ];
    assert_eq!(
        observations,
        [
            ("ownership", true),
            ("no_input", true),
            ("no_end", true),
            ("no_redecision", true),
            ("no_second_spawn", true),
            ("retained_spawning", true),
        ],
        "CleanupUnconfirmed must retain all five joined observations and prevent redecision"
    );

    running.stop(ExitCause::Ended);
    assert_eq!(
        host.owned(),
        1,
        "ordinary exit discarded ownership whose boundary coverage stayed incomplete"
    );
}

/// A confirmed host absence alone enters the supervisor as `SpawnFailed`; the following uncertain
/// control emits no end and stops the daemon from producing a second confirmed lifecycle.
#[test]
fn the_daemon_maps_only_confirmed_absence_to_spawn_failed() {
    let fixture = common::Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &fixture.settings(),
        common::seams(
            &settings,
            common::ConfirmedFailureHost::new(),
            Arc::new(common::ScriptedRunner::default()),
            Arc::new(common::TestClock::ticking(1_787_394_600_000, 1)),
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
    let ended = fixture.await_record("NotLaunched", |record| match record {
        Record::TurnEnded {
            turn,
            outcome: Some(TurnOutcome::NotLaunched { reason }),
            ..
        } if reason == "managed process could not launch (PermissionDenied)" => Some(*turn),
        _ => None,
    });
    fixture.await_record("launch failure hold", |record| match record {
        Record::AgentHeld {
            cause: dudes_proto::AutomaticHold::LaunchFailed { .. },
            ..
        } => Some(()),
        _ => None,
    });
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::TurnIssued { .. }))
            .count(),
        1
    );
    running.handle.input(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    fixture.await_record("later TurnIssued", |record| match record {
        Record::TurnIssued { turn, .. } if *turn != ended => Some(()),
        _ => None,
    });
    running.stop(ExitCause::Ended);

    let records = fixture.records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                Record::TurnEnded {
                    outcome: Some(TurnOutcome::NotLaunched { .. }),
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(
        records
            .iter()
            .all(|record| !matches!(record, Record::TurnSpawned { .. }))
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::TurnIssued { .. }))
            .count(),
        2,
        "the confirmed end admitted one later decision; its uncertain result emitted no input"
    );
}
