//! The operator credential exists after startup, and is created exactly once per daemon
//! run.
//!
//! # This is the mechanism's first and only production caller
//!
//! `LocalTransport::bind_in_with_credential` landed ahead of its caller and had, until this daemon, no call
//! site outside tests and doc comments. Startup step 4 is that call and step 5 is the check that it
//! happened.
//!
//! # The control discriminates, and this is *where* it discriminates
//!
//! This test asserts something that goes red **when the mint is removed, not merely when the
//! runtime directory is missing**. That distinction is real, because `bind_in` — the mutant —
//! establishes and verifies the same runtime directory through the same `establish` helper, so the
//! directory is present either way.
//!
//! **What fires is startup step 5, not the split assertions below.** Measured: with
//! `bind_in_with_credential` swapped for `bind_in`, `boot` returns `Err` at step 5 —
//! *"the transport bound without minting an operator credential"* — and both tests here die at
//! `.expect("boot")` before reaching a single assertion. So the daemon refuses to run at all, which
//! is a stronger answer than a red assertion and is red for the mint by name.
//!
//! The two facts are still asserted separately below, and that is not redundancy: they are what a
//! reader checks if step 5's refusal is ever weakened into a warning.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use duded::startup::{StartupStep, Witness};
use duded::{ExitCause, boot};
use dudes_proto::rpc::cred::{OPERATOR_CREDENTIAL_LEN, OperatorCredential};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

/// After startup the credential is there, owner-only, and well formed — and the runtime directory
/// is asserted separately so that "the directory exists" cannot stand in for "the mint happened".
#[test]
fn the_operator_credential_is_minted_once_and_is_owner_only() {
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
    .expect("boot");
    let mut running = common::start(daemon);

    // The two facts, separately. The first is what `bind_in` alone would also give.
    assert!(
        settings.runtime_dir.is_dir(),
        "the runtime directory was established"
    );
    let credential = settings
        .runtime_dir
        .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME);
    assert!(
        credential.is_file(),
        "and the credential was minted inside it"
    );

    let mode = std::fs::metadata(&credential)
        .expect("the credential")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode,
        dudes_rpc::operator_credential_permissions(),
        "owner-only"
    );

    let text = std::fs::read_to_string(&credential).expect("the credential text");
    assert_eq!(
        text.len(),
        OPERATOR_CREDENTIAL_LEN,
        "no newline, nothing else: a reader is a read and a comparison, not a parser"
    );
    OperatorCredential::new(text.clone()).expect("it is base64url of the right width");

    // Once per daemon run: the recorder holds exactly one mint step.
    let mints = witness
        .startup
        .steps()
        .iter()
        .filter(|step| **step == StartupStep::MintCredential)
        .count();
    assert_eq!(mints, 1, "the mint is a step, and it happens once");

    // And the mechanism itself refuses a second one in the same directory — `create_new`.
    let second = OperatorCredential::new("C".repeat(OPERATOR_CREDENTIAL_LEN)).expect("well formed");
    let error = dudes_rpc::LocalTransport::bind_in_with_credential(
        &settings.runtime_dir,
        "another.sock",
        &second,
    )
    .map(|_| ())
    .expect_err("a second credential in the same directory is refused");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
    assert_eq!(
        std::fs::read_to_string(&credential).expect("the credential"),
        text,
        "and the first daemon's credential is untouched"
    );

    running.stop(ExitCause::Ended);
}

/// A daemon that starts, stops and starts again in the same runtime directory mints again — which
/// is what "once per daemon **run**" means, and what makes the reclaim in startup step 4 load
/// bearing rather than a convenience.
///
/// POSIX unlink keeps an open handle attached to the unlinked file object while allowing a new
/// object at the same path. Truncating run 1's retained handle after run 2 boots therefore proves
/// the path names a distinct object when its credential stays complete. Bytes cannot witness that
/// because `FakeRng` repeats them, timestamps can share a filesystem tick, and Linux may reuse an
/// inode immediately after unlink.
#[test]
fn a_second_run_mints_a_second_credential() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let credential = settings
        .runtime_dir
        .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME);

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
    let first_credential = std::fs::OpenOptions::new()
        .write(true)
        .open(&credential)
        .expect("hold the first run's credential open");
    first.stop(ExitCause::Ended);
    assert!(
        credential.exists(),
        "control: a clean exit leaves the credential behind — `bind` creates and never removes, \
         which is exactly why startup step 4 has to reclaim it"
    );

    let second = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::ticking(1_787_394_700_000, 1)),
            Witness::default(),
        ),
    )
    .expect("the same runtime directory boots again");
    let mut second = common::start(second);
    first_credential
        .set_len(0)
        .expect("truncate the first run's still-open credential");
    assert_eq!(
        first_credential.metadata().expect("the open file").len(),
        0,
        "control: the retained handle really was truncated"
    );
    let current = std::fs::read_to_string(&credential).expect("the second run's credential");
    assert_eq!(
        current.len(),
        OPERATOR_CREDENTIAL_LEN,
        "the path names the second run's complete credential, not the truncated first file"
    );
    OperatorCredential::new(current).expect("the second run's credential is well formed");
    second.stop(ExitCause::Ended);
}
