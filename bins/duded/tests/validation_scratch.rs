//! Completed-contract daemon startup selects scratch storage in its state directory.
#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_journal::{Fsync, JournalOptions};
use dudes_proto::record::{Hex32, samples};
use dudes_proto::{
    Entry, Gate, GateStep, LoopDecision, MailFrom, MailRef, MailTo, MailWakeAdmission,
    MailWakeDecision, MailWakeDigest, MailWakeDigestDomain, MailWakePlan, MailWakePurpose,
    MailWakeTarget, MailWakeTask, Record, RecordKind,
};
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_types::{MailState, Timestamp};

mod common;

fn seed_completed_contract(directory: &Path) {
    let (mut writer, _) = dudes_journal::open(
        directory,
        JournalOptions {
            fsync: Fsync::None,
            checkpoint_every: 0,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    let mut append = |record| {
        writer
            .append(&Entry {
                at: Timestamp::UNIX_EPOCH,
                record,
            })
            .unwrap()
    };
    let hired = samples::record(RecordKind::AgentHired);
    let Record::AgentHired { id: agent, .. } = hired else {
        unreachable!();
    };
    append(hired);
    let goal = samples::record(RecordKind::GoalSet);
    let Record::GoalSet { goal: spec, .. } = &goal else {
        unreachable!();
    };
    let goal_id = spec.id;
    append(goal);
    append(samples::record(RecordKind::PredicateResult));
    let completion_lsn = append(Record::LoopDecided {
        agent,
        input_digest: "completed".into(),
        gate_trace: Vec::new(),
        decision: LoopDecision::Done,
    });
    let mut mail = samples::record(RecordKind::Mail);
    let Record::Mail {
        id,
        from,
        to,
        state,
        ..
    } = &mut mail
    else {
        unreachable!();
    };
    let id = *id;
    *from = MailFrom::Human;
    *to = MailTo::Agent { agent };
    *state = MailState::Durable;
    let reference = MailRef {
        id,
        lsn: append(mail),
    };
    let task = MailWakeTask::new(1).unwrap();
    append(Record::MailWakeTurnDecided(MailWakeDecision {
        agent,
        goal: goal_id,
        completion_lsn,
        purpose: MailWakePurpose::FreshPreparation {
            plan: MailWakePlan {
                trigger: reference,
                frontier: reference.lsn,
                unread_count: 1,
                task: task.clone(),
                targets: vec![MailWakeTarget {
                    mail: reference,
                    original_count: 0,
                    target_count: 1,
                }],
            },
        },
        input_digest: MailWakeDigest {
            domain: MailWakeDigestDomain::Fresh,
            value: Hex32::new([1; 32]),
        },
        gate_trace: Gate::ORDER
            .into_iter()
            .map(|gate| GateStep {
                gate,
                fired: gate == Gate::Work,
                note: None,
            })
            .collect(),
        decision: MailWakeAdmission::Issue { task },
    }));
}

#[test]
fn daemon_boot_uses_state_scratch_when_ambient_temp_is_unusable() {
    let fixture = common::Fixture::new();
    seed_completed_contract(&fixture.journal_dir());
    let missing = fixture.state.path().join("missing-ambient-temp");
    let spec = LaunchSpec {
        argv: vec![
            std::env::current_exe().unwrap().into_os_string(),
            "--exact".into(),
            "daemon_scratch_child".into(),
            "--nocapture".into(),
        ],
        env: vec![
            EnvVar::new("TMPDIR", missing.as_os_str()),
            EnvVar::new("TMP", missing.as_os_str()),
            EnvVar::new("TEMP", missing.as_os_str()),
            EnvVar::new("DUDES_SCRATCH_BOOT_STATE", fixture.state.path().as_os_str()),
            EnvVar::new(
                "DUDES_SCRATCH_BOOT_RUNTIME",
                fixture.runtime.path().as_os_str(),
            ),
        ],
        cwd: fixture.state.path().to_path_buf(),
        stdin: LaunchInput::Null,
    };
    let output = dudes_host::OneShot::default()
        .run_captured(&spec, Duration::from_secs(30), 64 * 1024)
        .unwrap();
    assert!(
        matches!(
            output,
            dudes_host::CapturedOutcome::Finished {
                exit: Some(0),
                timed_out: false,
                truncated: false,
                ..
            }
        ),
        "{output:?}"
    );
    assert!(!missing.exists());
    assert!(
        fixture
            .state
            .path()
            .join(duded::boot::PROJECTION_FILE)
            .exists()
    );
}

#[test]
fn daemon_scratch_child() {
    let Some(state) = std::env::var_os("DUDES_SCRATCH_BOOT_STATE") else {
        return;
    };
    let runtime = std::env::var_os("DUDES_SCRATCH_BOOT_RUNTIME").unwrap();
    assert!(!std::env::temp_dir().exists());
    let settings = common::settings_at(Path::new(&state), Path::new(&runtime));
    let daemon = boot(
        &settings,
        duded::Seams {
            host: common::SharedHost::new(),
            pty_host: Box::new(duded::ports::ClosedPtyHost),
            predicates: Arc::new(common::ScriptedRunner::default()),
            clock: Arc::new(common::TestClock::new()),
            rng: dudes_supervisor::testing::FakeRng::new(),
            resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
            adapters: dudes_providers::registry::Adapters::new([Box::new(
                dudes_providers::claude::ClaudeAdapter::new(
                    dudes_providers::claude::config::ClaudeConfig::default(),
                ),
            )
                as Box<dyn dudes_providers::adapter::Adapter>]),
            children: duded::ChildEnds::new(),
            witness: Witness::default(),
        },
    )
    .expect("boot validates completed history in its state directory");
    let mut running = common::start(daemon);
    running.stop(ExitCause::Ended);
}
