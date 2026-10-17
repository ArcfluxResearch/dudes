//! The seven startup steps happen in order, and a second daemon on the same state
//! directory is refused.
//!
//! # Two mechanisms, and the honest ranking of them
//!
//! The order is **structural**: `boot` is a chain whose steps take one another's products by value,
//! so a genuine reorder does not compile. That is the guarantee, and it is not what this file
//! asserts — a compile error is not a runtime assertion.
//!
//! What this file asserts is the **recorder**, which is weaker in a way worth stating plainly:
//! because a real reorder fails to compile, the mutation that reddens the sequence assertion is
//! *moving a `record` call*, not moving the logic. So the recorder is evidence that each step ran
//! and that each one's on-disk artefact exists beside it — not the thing enforcing the order.
//!
//! # The refusal is attributed, not merely observed
//!
//! Two later steps refuse a second daemon for reasons of their own: `bind` refuses any existing
//! path, and the credential mint is `create_new`. A test that only asserted "the second daemon
//! failed" would pass against a daemon whose lock does nothing. So it asserts the second daemon's
//! recorder holds **zero** steps — it never reached `JournalOpen`, which means step 1 is what
//! refused it.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::{fs, path::Path};

use duded::startup::{StartupStep, Witness};
use duded::{ExitCause, boot};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
);

fn floor_fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("Floor fixture");
    fs::create_dir(root.path().join("assets")).expect("assets");
    for path in [
        "index.html",
        "release-manifest.json",
        "assets/app-01234567.js",
    ] {
        fs::copy(Path::new(FLOOR_FIXTURE).join(path), root.path().join(path)).expect("copy");
    }
    root
}

/// Boot, assert, and take the daemon down through its own exit sequence so no thread outlives the
/// test.
macro_rules! shut {
    ($daemon:expr) => {{
        let handle = $daemon.handle();
        handle.stop(ExitCause::Ended);
        $daemon.run().expect("the exit sequence")
    }};
}

/// The seven steps, in order, each with the artefact it claims to have produced.
///
/// `ReaperStart` has no on-disk artefact and `FirstDecide` has none on a *fresh* directory — an
/// `Input::Started` with no agents to decide for journals nothing. Both are named rather than
/// asserted by proxy here; `FirstDecide` gets its artefact in
/// [`the_first_decide_writes_a_decision_when_there_is_an_agent_to_decide_for`].
#[test]
fn the_seven_startup_steps_happen_in_order() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let witness = Witness::default();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            witness.clone(),
        ),
    )
    .expect("a fresh directory boots");

    assert_eq!(
        witness.startup.steps(),
        StartupStep::ORDER.to_vec(),
        "the seven steps, in order"
    );

    // Step 1's artefact.
    assert!(
        settings
            .state_dir
            .join(duded::lock::LOCK_FILE_NAME)
            .exists(),
        "the lock file is in the state directory"
    );
    // Step 2's.
    assert!(
        std::fs::read_dir(fixture.journal_dir())
            .expect("the journal directory")
            .next()
            .is_some(),
        "the journal directory has a segment in it"
    );
    // Step 3's: the projection store exists, which is the file replay folds into.
    assert!(
        settings
            .state_dir
            .join(duded::boot::PROJECTION_FILE)
            .exists(),
        "the projection store is open"
    );
    // Step 4's — and `symlink_metadata`, never `metadata`: the question is about the file at this
    // path, and following the last component would answer about a different one.
    let socket = settings.socket();
    let kind = std::fs::symlink_metadata(&socket).expect("the socket");
    assert!(
        std::os::unix::fs::FileTypeExt::is_socket(&kind.file_type()),
        "a real socket stands at {}",
        socket.display()
    );
    // Step 5's.
    let credential = settings
        .runtime_dir
        .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME);
    assert!(credential.exists(), "the operator credential was minted");

    shut!(daemon);
}

/// The first decide is a decide: with an agent that has a goal on the disk, restarting writes a
/// `LoopDecided` before anything else this run does.
///
/// The control is the fresh-directory arm above, which records `FirstDecide` and writes **no**
/// decision — so this assertion cannot pass on a daemon that journals a decision for no reason.
#[test]
fn the_first_decide_writes_a_decision_when_there_is_an_agent_to_decide_for() {
    let fixture = Fixture::new();
    let settings = fixture.settings();

    // First run: hire an agent and give it a goal, so the journal carries one to decide for.
    let first = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut first = common::start(first);
    let agent = common::hire(&first.handle, fixture.state.path());
    // Paused before the goal, so the agent is decidable but no turn is issued: what this test wants
    // on the disk is an agent with a goal and *no open turn*, which is the state a restart's first
    // decide has something to say about.
    first
        .handle
        .ask(duded::ports::Ask::Pause(agent))
        .expect("the pause");
    common::set_goal(
        &first.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    first.stop(ExitCause::Ended);

    let before = fixture
        .records()
        .iter()
        .filter(|record| matches!(record, dudes_proto::Record::LoopDecided { .. }))
        .count();
    assert!(before > 0, "control: the first run decided at least once");

    // Second run: the same directories, so replay finds the agent and the goal.
    let witness = Witness::default();
    let second = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::ticking(1_787_394_700_000, 1)),
            witness.clone(),
        ),
    )
    .expect("the same directories boot again once the first daemon has gone");
    let mut second = common::start(second);
    assert_eq!(witness.startup.steps(), StartupStep::ORDER.to_vec());
    let after = fixture
        .records()
        .iter()
        .filter(|record| matches!(record, dudes_proto::Record::LoopDecided { .. }))
        .count();
    assert!(
        after > before,
        "the first decide of the second run wrote a decision: {before} -> {after}"
    );
    second.stop(ExitCause::Ended);
}

/// Invariant 1: a second daemon on the same state directory is refused — **by step 1**.
///
/// Three assertions, and the third is the one that matters. The refusal itself would also be
/// produced by `bind` (which refuses any existing path) or by the credential mint (`create_new`),
/// so the second daemon's recorder is read: zero steps means it never reached `JournalOpen`, which
/// means the lock is what stopped it.
///
/// The control is the in-tree idiom for a once-per-directory assertion: the same boot into a
/// **fresh** directory succeeds, so the test cannot pass on a daemon that refuses to start at all.
#[test]
fn a_second_daemon_on_the_same_state_directory_is_refused_by_the_lock() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let first = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("the first daemon boots");

    // A different runtime directory, so the socket and the credential cannot be what refuses it.
    let elsewhere = tempfile::tempdir().expect("a second runtime directory");
    let second_settings = duded::Settings {
        runtime_dir: elsewhere.path().join("run"),
        ..settings.clone()
    };
    let witness = Witness::default();
    let error = boot(
        &second_settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            witness.clone(),
        ),
    )
    .map(|_| ())
    .expect_err("a second daemon on the same state directory is refused");
    assert!(
        format!("{error:#}").contains("single-writer lock"),
        "refused for the wrong reason: {error:#}"
    );
    assert!(
        witness.startup.steps().is_empty(),
        "the refusal came from step 1: the second daemon recorded {:?}",
        witness.startup.steps()
    );

    // Control: a fresh state directory boots, so the refusal is about *this* directory.
    let other = Fixture::new();
    let third = boot(
        &other.settings(),
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("control: a fresh state directory boots while the first daemon holds its own");
    shut!(third);
    shut!(first);
}

#[test]
fn enabled_floor_prepares_before_first_decide_and_binds_after_it() {
    let fixture = Fixture::new();
    let floor = floor_fixture();
    let witness = Witness::default();
    let mut settings = fixture.settings();
    settings.floor = Some(duded::settings::FloorSettings {
        assets_dir: floor.path().to_path_buf(),
        port: Some(0),
    });
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            witness.clone(),
        ),
    )
    .expect("enabled Floor boot");
    assert_eq!(witness.startup.steps(), StartupStep::FLOOR_ORDER.to_vec());
    assert!(daemon.floor_addr().expect("Floor address").is_ipv4());
    shut!(daemon);
}

#[test]
fn missing_and_mixed_floor_assets_refuse_before_listener_bind() {
    for mutate in ["missing", "mixed"] {
        let fixture = Fixture::new();
        let floor = floor_fixture();
        match mutate {
            "missing" => {
                fs::remove_file(floor.path().join("assets/app-01234567.js")).expect("remove asset");
            }
            "mixed" => fs::write(
                floor.path().join("assets/app-01234567.js"),
                b"different release bytes\n",
            )
            .expect("mix asset"),
            _ => unreachable!(),
        }
        let witness = Witness::default();
        let mut settings = fixture.settings();
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .expect("reserve")
            .local_addr()
            .expect("address")
            .port();
        settings.floor = Some(duded::settings::FloorSettings {
            assets_dir: floor.path().to_path_buf(),
            port: Some(port),
        });
        let result = boot(
            &settings,
            seams(
                &settings,
                SharedHost::new(),
                Arc::new(ScriptedRunner::default()),
                Arc::new(TestClock::new()),
                witness.clone(),
            ),
        );
        assert!(result.is_err(), "{mutate} bundle must be refused");
        assert_eq!(
            witness.startup.steps(),
            vec![
                StartupStep::Lock,
                StartupStep::JournalOpen,
                StartupStep::Replay
            ]
        );
        assert!(!settings.socket().exists(), "control door never bound");
        assert!(
            std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
            "Floor port was never bound"
        );
    }

    let fixture = Fixture::new();
    let floor = floor_fixture();
    let mut settings = fixture.settings();
    settings.floor = Some(duded::settings::FloorSettings {
        assets_dir: floor.path().to_path_buf(),
        port: Some(0),
    });
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("valid control starts");
    shut!(daemon);
}

#[test]
fn a_labelled_post_reader_fold_fault_stops_before_replay_completion() {
    for fault in [false, true] {
        let fixture = Fixture::new();
        let mut settings = fixture.settings();
        let seed = boot(
            &settings,
            seams(
                &settings,
                SharedHost::new(),
                Arc::new(ScriptedRunner::default()),
                Arc::new(TestClock::new()),
                Witness::default(),
            ),
        )
        .unwrap();
        let mut seed = common::start(seed);
        common::hire(&seed.handle, fixture.state.path());
        seed.stop(ExitCause::Ended);
        let (reader, _) = dudes_journal::Reader::open(&fixture.journal_dir()).unwrap();
        let prefix: Vec<_> = reader
            .iter_from(dudes_types::Lsn::ZERO)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let lsn = prefix
            .iter()
            .find_map(|(lsn, entry)| {
                matches!(entry.record, dudes_proto::Record::AgentHired { .. }).then_some(*lsn)
            })
            .unwrap();
        let runtime = tempfile::tempdir().unwrap();
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
        settings.runtime_dir = runtime.path().to_path_buf();
        let witness = Witness::default();
        let host = SharedHost::new();
        if fault {
            witness.refuse_replay_fold_once(
                lsn,
                dudes_proto::recovery_state::RecoveryStateError::Binding,
            );
        }
        let result = boot(
            &settings,
            seams(
                &settings,
                host.clone(),
                Arc::new(ScriptedRunner::default()),
                Arc::new(TestClock::new()),
                witness.clone(),
            ),
        );
        if fault {
            let Err(failure) = result else {
                panic!("the labelled fold fault must stop boot");
            };
            assert_eq!(
                failure.downcast_ref::<dudes_supervisor::input::DurableInputError>(),
                Some(&dudes_supervisor::input::DurableInputError::Recovery {
                    lsn,
                    reason: dudes_proto::recovery_state::RecoveryStateError::Binding
                })
            );
            assert_eq!(
                witness.startup.steps(),
                vec![StartupStep::Lock, StartupStep::JournalOpen]
            );
            assert!(!settings.socket().exists());
            assert!(
                !settings
                    .runtime_dir
                    .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME)
                    .exists()
            );
            assert!(host.spawned().is_empty());
            assert_eq!(fixture.entries(), prefix);
        } else {
            let daemon = result.unwrap();
            assert_eq!(witness.startup.steps(), StartupStep::ORDER.to_vec());
            assert!(host.spawned().is_empty());
            shut!(daemon);
        }
    }
}
