//! A launched child's environment carries the socket path and the agent's token, and the
//! token's plaintext appears nowhere in the journal while its hash does.
//!
//! # The environment, and why it is not argv
//!
//! `ps` shows argv. Both variables go on `LaunchSpec.env`, whose values are `EnvVar`s
//! that redact themselves, and never into `LaunchSpec.argv`.
//!
//! # The absence half needs its own control, and this is where it is built
//!
//! **Scanning every journal frame for the plaintext proves nothing until the scan has been shown to
//! find one.** So the scan is a function, [`journal_holds`], and
//! [`the_scan_can_find_a_plaintext_it_is_given`] plants a token into a fixture journal and asserts
//! it fires. Without that, a scan looking at the wrong bytes — or at no bytes — would report the
//! same clean answer for ever.
#![cfg(unix)]

use std::sync::Arc;

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::Record;
use dudes_supervisor::host::blake3_hex;

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

/// Whether the raw bytes of any journal segment contain `needle`.
///
/// Reads the **files**, not the parsed records: a plaintext that leaked into a field nothing models
/// would still be bytes on the disk, and a scan over typed records would not see it.
fn journal_holds(journal_dir: &std::path::Path, needle: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(journal_dir) else {
        return false;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .any(|entry| {
            let Ok(bytes) = std::fs::read(entry.path()) else {
                return false;
            };
            bytes
                .windows(needle.len())
                .any(|window| window == needle.as_bytes())
        })
}

/// The scan can fail, in the shape it exists to catch.
///
/// A fixture directory with a file whose bytes carry the needle: the scan reports it. Then a
/// directory without it: the scan reports nothing. Both arms, because "finds everything" and
/// "finds nothing" are both wrong and one arm cannot tell them apart.
#[test]
fn the_scan_can_find_a_plaintext_it_is_given() {
    let dir = tempfile::tempdir().expect("tempdir");
    let planted = "A".repeat(43);
    std::fs::write(
        dir.path().join("000001.log"),
        format!("some frame bytes {planted} and more").as_bytes(),
    )
    .expect("write");
    assert!(
        journal_holds(dir.path(), &planted),
        "the scan finds a planted plaintext"
    );
    assert!(
        !journal_holds(dir.path(), &"B".repeat(43)),
        "and does not find one that is not there"
    );
}

/// The child's environment carries both variables, neither reaches argv, and the journal holds the
/// hash and not the plaintext.
#[test]
fn a_launched_child_carries_the_socket_and_a_token_that_is_never_journaled() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::new();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    fixture.await_turn();
    running.stop(ExitCause::Ended);

    let specs = host.spawned();
    let spec = specs.first().expect("the daemon launched a child");

    let value_of = |name: &str| {
        spec.env
            .iter()
            .find(|var| var.name() == std::ffi::OsStr::new(name))
            .map(|var| var.expose_value().to_string_lossy().into_owned())
    };
    let socket = value_of("DUDES_SOCK").expect("DUDES_SOCK is on the child's environment");
    assert_eq!(
        socket,
        settings.socket().to_string_lossy(),
        "and it is the socket this daemon actually bound"
    );
    let token = value_of("DUDES_TOKEN").expect("DUDES_TOKEN is on the child's environment");
    assert!(!token.is_empty());

    // Never argv, because `ps` shows argv. The non-empty guard is the control: the
    // loop below is an absence assertion, and an empty argv would satisfy it vacuously — the
    // `value_of` reads above are of `spec.env` and say nothing about this.
    assert!(
        !spec.argv.is_empty(),
        "control: the launch has an argv for the assertion below to be about"
    );
    for part in &spec.argv {
        let part = part.to_string_lossy();
        assert!(!part.contains(&token), "the token reached argv: {part}");
        assert!(
            !part.contains("DUDES_TOKEN"),
            "the token's name reached argv: {part}"
        );
    }

    // The hash is journaled; the plaintext is not.
    let records = fixture.records();
    let hash = records
        .iter()
        .find_map(|record| match record {
            Record::HookTokenMinted { token_hash, .. } => Some(token_hash.clone()),
            _ => None,
        })
        .expect("the mint was journaled");
    assert_eq!(
        hash,
        blake3_hex(token.as_bytes()),
        "the journaled hash is blake3 of the plaintext the child was given"
    );
    assert!(
        journal_holds(&fixture.journal_dir(), &hash),
        "control: the scan does find the hash, so it is reading the right bytes"
    );
    assert!(
        !journal_holds(&fixture.journal_dir(), &token),
        "the token's plaintext is held for the daemon run only and is never journaled"
    );
}
