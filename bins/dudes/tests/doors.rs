//! The two doors, over a real socket, with the real classifier.
//!
//! # The four cases this file holds, and how they discriminate
//!
//! 1. **The binary holding no credential — the admitting arm.** `system.ping` completes and
//!    deserialises into the contract's response type. This is the positive
//!    control: a refusal-only test cannot tell a working classifier from a broken transport.
//! 2. **The binary holding no credential — the refusing arm.** `hire`, `goal`, `why` and `tail`
//!    each answer `-32001 Unauthorized`, and the test fails if any one of them succeeds.
//! 3. **The binary holding the credential.** The same four are admitted, on the **same daemon**,
//!    through the **same registrations**. Only the credential's readability differs.
//! 4. **The binary holding what a managed child holds.** The `DUDES_SOCK` and token pair and an inherited `HOME`,
//!    with no `DUDES_RUNTIME_DIR` — so the credential is reached through the `$HOME` fallback rather
//!    than through a directory the test named. Against `duded::classify::NoWalk`, the resolver that
//!    ships, because the fixture's default column decides identically and would prove nothing about
//!    it. This exposure is accepted.
//!
//! The admitting arm is the control for the refusing arm. That pairing is what makes the
//! refusal evidence: *"refused because nothing is registered"* cannot survive it, because the second
//! half is admitted through the very registrations the first half was refused by.
//!
//! # What is cited and what is not restated
//!
//! `classify` decides what a presentation resolves to; each method's `Requirement` says what each
//! role may call. No cell is copied here. `-32001` is not a cell of either — presenting nothing
//! always resolves to `Unproven` and a `Requirement` carries roles, not error codes — it is
//! *derived*: the peer resolves to `Unproven`, these four methods admit only `Operator`,
//! and the refusal follows. `crates/dudes-rpc/tests/` already asserts the same shape at
//! the crate level; this file is its CLI-level analogue.
//!
//! # The narrowing this file inherits, which is now about the process and not the seam
//!
//! The daemon below is composed exactly as `main.rs` composes one — the shipped `PeerClassifier`,
//! over the credential it minted — but it runs on a thread of this test rather than out of
//! `env!("CARGO_BIN_EXE_duded")`, and its resolver is handed in so a column can be named. So a
//! green run is evidence about the daemon's composition and its registrations, and not about the
//! binary's own argv and environment handling. `bins/duded/tests/wire.rs` carries the same
//! statement from the daemon's side.
#![cfg(unix)]

use std::sync::Arc;

use duded::settings::Settings;
use dudes::exit;
use dudes_proto::rpc::ErrorCode;
use dudes_proto::rpc::agent::HireResult;
use dudes_proto::rpc::hook::HOOK_TOKEN_LEN;
use dudes_proto::rpc::system::PingResult;

mod common;

use common::{Cli, Fixture, OpenRunner, SharedHost, seams_with, start, start_with};

/// Every command that is an `Operator` row, as this CLI spells it, paired
/// with the row number it is.
///
/// One list, used by the refusing arm and by the admitting arm, so the two cannot drift apart into
/// testing different sets — which is the way a paired control quietly stops being one.
fn operator_commands(
    cwd: &std::path::Path,
    agent: &str,
) -> Vec<(&'static str, Vec<std::ffi::OsString>)> {
    vec![
        ("agent.hire", common::hire_args(cwd)),
        ("goal.set", common::goal_args(agent, "make the tests pass")),
        (
            "agent.why",
            ["why", "--agent", agent]
                .iter()
                .map(std::ffi::OsString::from)
                .collect(),
        ),
        (
            "journal.tail",
            ["tail", "--from", "0"]
                .iter()
                .map(std::ffi::OsString::from)
                .collect(),
        ),
    ]
}

/// An agent id that is well formed and names nobody, for the arms that must be refused **before**
/// the handler could notice that.
///
/// That order is what makes the refusal mean something: a refused call never reaches a handler, so
/// `agent.why` on a stranger answers `-32001` and not `-32003 NotFound`. Asserting the code rather
/// than merely "an error" is what separates the two.
const A_STRANGER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

/// One daemon, both arms of the no-credential clause, and then the credential clause that controls
/// it — in that order, on one process, because the *pairing* is the assertion.
#[test]
fn the_four_operator_commands_turn_on_the_credential_and_ping_never_does() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::new();
    let mut running = start(&settings, host.clone(), Arc::new(OpenRunner::default()));
    let socket = settings.socket();

    // --- arm 1: the admitting arm, which is this file's positive control --------------------
    // `DUDES_RUNTIME_DIR` points at a directory that holds no credential, so the CLI presents
    // nothing: it resolves to `Unproven`.
    let ping = Cli::new(&socket, fixture.empty.path())
        .args(["--json", "ping"])
        .run();
    assert_eq!(
        ping.code(),
        exit::SUCCESS,
        "a peer presenting nothing reaches system.ping\n{}",
        ping.output()
    );
    let answered: PingResult = ping.json();
    assert_eq!(
        answered.socket, socket,
        "and it is *this* daemon answering, not a stale one from another test"
    );

    // --- arm 2: the refusing arm -----------------------------------------------------------
    for (row, args) in operator_commands(fixture.state.path(), A_STRANGER) {
        let refused = Cli::new(&socket, fixture.empty.path())
            .arg("--json")
            .args(&args)
            .run();
        assert_eq!(
            refused.code(),
            exit::REFUSED,
            "{row} must be refused to a peer presenting nothing\n{}",
            refused.output()
        );
        assert_eq!(
            refused.json_error().code,
            ErrorCode::Unauthorized.code(),
            "{row}: the refusal is the authorisation one, not a handler's\n{}",
            refused.output()
        );
    }

    // The daemon is still serving: a refusal is a refusal and not a stop.
    assert_eq!(
        Cli::new(&socket, fixture.empty.path())
            .args(["ping"])
            .run()
            .code(),
        exit::SUCCESS,
        "the refusals left the daemon serving"
    );
    assert_eq!(
        host.children(),
        0,
        "and nothing was hired, so no child exists yet"
    );

    // --- arm 3: the same four, with the credential readable ---------------------------------
    // Same daemon, same registrations, same commands. The **only** difference is which directory
    // the CLI reads its credential from.
    let hired = Cli::new(&socket, &settings.runtime_dir)
        .arg("--json")
        .args(common::hire_args(fixture.state.path()))
        .run();
    assert_eq!(
        hired.code(),
        exit::SUCCESS,
        "agent.hire is admitted with the credential\n{}",
        hired.output()
    );
    let agent = hired.json::<HireResult>().agent.to_string();

    for (row, args) in operator_commands(fixture.state.path(), &agent) {
        let admitted = Cli::new(&socket, &settings.runtime_dir)
            .arg("--json")
            .args(&args)
            .run();
        assert_eq!(
            admitted.code(),
            exit::SUCCESS,
            "{row} is admitted with the credential\n{}",
            admitted.output()
        );
    }

    running.stop();
}

/// The discriminator, isolated: **the same command, the same daemon, two runs that differ only in
/// whether the credential is readable.**
///
/// The test must fail if removing the credential leaves them admitted, and this is the
/// arm that states it as one assertion rather than as two halves of a longer test. It is not
/// redundant with the test above: that one proves each of the four commands works both ways round,
/// this one proves the *difference* is the credential and nothing else that changed along the way —
/// no ordering, no accumulated state, no second daemon.
#[test]
fn the_only_difference_between_admitted_and_refused_is_the_credential() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let mut running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let socket = settings.socket();
    let args = common::hire_args(fixture.state.path());

    let with = Cli::new(&socket, &settings.runtime_dir)
        .arg("--json")
        .args(&args)
        .run();
    let without = Cli::new(&socket, fixture.empty.path())
        .arg("--json")
        .args(&args)
        .run();

    assert_eq!(
        with.code(),
        exit::SUCCESS,
        "with the credential\n{}",
        with.output()
    );
    assert_eq!(
        without.code(),
        exit::REFUSED,
        "without it\n{}",
        without.output()
    );
    assert_ne!(
        with.code(),
        without.code(),
        "the credential is the whole difference"
    );

    running.stop();
}

/// A credential file that exists and holds the **wrong** text resolves to row 4 like any other
/// unrecognised presentation — not to an admission, and not to a local failure that never asked.
///
/// # The admitting arm is inside this test, and that is deliberate
///
/// Relying on the two tests above as its controls would not discriminate:
/// under a mutation making the daemon's classifier fail to construct at all — so every call is
/// refused — its two siblings would go red and *this one would pass*, because nextest runs each test in its
/// own process and under a filter it stands alone asserting only "some error came back". A
/// refuse-everything daemon would satisfy it.
///
/// That is a discrimination gap rather than a construction bug: the
/// classifier is built on this path, and a test standing alone could not tell the two worlds apart.
///
/// So the arm below runs first, on **this** fixture and **this** daemon: the credential the daemon
/// actually minted is admitted. That is what makes the refusal underneath it mean "the wrong
/// credential is not a credential" rather than "this daemon refuses everything".
///
/// The test deliberately stays in this file rather than moving to one of its own: its siblings are
/// the same construction path, and separating them would hide that.
#[test]
fn a_credential_the_daemon_does_not_recognise_is_refused() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let mut running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let socket = settings.socket();

    // --- the in-test positive control: this daemon's own credential is admitted ------------------
    let admitted = Cli::new(&socket, &settings.runtime_dir)
        .arg("--json")
        .args(common::hire_args(fixture.state.path()))
        .run();
    assert_eq!(
        admitted.code(),
        exit::SUCCESS,
        "control: this daemon admits the credential it minted, so it is not refusing everything\n{}",
        admitted.output()
    );

    // Well formed — 43 base64url characters — and not this daemon's. A malformed one would be
    // refused on the way in by the envelope's own validation, which would test a different thing.
    let elsewhere = tempfile::tempdir().expect("a directory");
    std::fs::write(
        elsewhere.path().join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        "z".repeat(dudes_proto::rpc::cred::OPERATOR_CREDENTIAL_LEN),
    )
    .expect("write a stranger's credential");

    let refused = Cli::new(&socket, elsewhere.path())
        .arg("--json")
        .args(common::hire_args(fixture.state.path()))
        .run();
    assert_eq!(
        refused.code(),
        exit::REFUSED,
        "a credential this daemon did not mint is not a credential\n{}",
        refused.output()
    );
    assert_eq!(
        refused.json_error().code,
        ErrorCode::Unauthorized.code(),
        "{}",
        refused.output()
    );

    running.stop();
}

/// The accepted exposure, at the exit code: a peer holding **only** the `DUDES_SOCK` and token
/// pair and an inherited `HOME` reaches `agent.hire`, whose `Requirement` is `Operator`.
///
/// # Why this is not a restatement of the three arms above
///
/// Each of those hands the CLI a `DUDES_RUNTIME_DIR` — the operator's arrangement — and varies what
/// sits in it. This one never sets that variable. The runtime directory is *derived*, by the
/// production resolver, from the `HOME` the child inherited, and the daemon is then bound exactly
/// where that resolver points. So what is asserted is the composition that
/// the daemon's settings make unavoidable (see `bins/duded/src/settings.rs`) — `dirname($DUDES_SOCK)` is the directory
/// the credential is in — and not a directory a test chose.
///
/// # The resolver is the one that ships, and that is the load-bearing half
///
/// `common::seams`'s default answers `PeerProvenance::NoRecordedAgent`, which shares the
/// operator-credential `Operator` arm with `Unresolvable` (`crates/dudes-rpc/src/auth.rs:617-620`). An arm run
/// against it would go green while saying nothing about the daemon's own resolver, so this one
/// hands in `duded::classify::NoWalk` — what `bins/duded/src/main.rs:236` installs.
///
/// # The control, and what it rules out
///
/// Two runs on one daemon, differing in **`HOME` alone**: the second names a directory with no
/// credential under it and is refused `-32001`. Without it, a daemon admitting everything would
/// satisfy the first assertion. `DUDES_SOCK`, `DUDES_TOKEN` and the argument line are byte-identical
/// across the pair, so the `$HOME` fallback is the whole difference.
#[test]
fn a_child_with_only_the_managed_pair_and_home_reaches_the_operator_table() {
    let fixture = Fixture::new();
    let home = tempfile::tempdir().expect("a home directory");
    let stranger = tempfile::tempdir().expect("a home directory with no credential under it");

    // Where a CLI holding this `HOME` and no `DUDES_RUNTIME_DIR` will look — answered by the
    // production resolver rather than by a literal, so the daemon below is bound at whatever
    // `Environment::resolve` says the default is. `bins/dudes/src/env.rs`'s own test module pins
    // the shape of that answer; this pins what reaching it costs.
    let home_var = home.path().as_os_str().to_owned();
    let runtime_dir =
        dudes::Environment::resolve(|name| (name == "HOME").then(|| home_var.clone()))
            .runtime_dir()
            .expect("an absolute HOME names the default runtime directory")
            .to_path_buf();
    // Only the parent: `bind_in` creates the last component itself and refuses one that already
    // exists with the wrong mode.
    std::fs::create_dir_all(runtime_dir.parent().expect("the default is not a root"))
        .expect("the home's state directory");

    let settings = Settings {
        runtime_dir,
        ..fixture.settings()
    };
    let mut running = start_with(
        &settings,
        seams_with(
            &settings,
            SharedHost::new(),
            Arc::new(OpenRunner::default()),
            Arc::new(duded::classify::NoWalk),
        ),
    );
    let socket = settings.socket();
    let token = "t".repeat(HOOK_TOKEN_LEN);

    // Exactly the pair, plus the `HOME` a managed child inherits. Nothing else: no
    // `DUDES_RUNTIME_DIR`, no `PATH`.
    let child = Cli::bare()
        .env("DUDES_SOCK", &socket)
        .env("DUDES_TOKEN", &token)
        .arg("--json")
        .args(common::hire_args(fixture.state.path()));

    let admitted = child.clone().env("HOME", home.path()).run();
    assert_eq!(
        admitted.code(),
        exit::SUCCESS,
        "a tokened child reaches agent.hire through the $HOME fallback\n{}",
        admitted.output()
    );
    // An `Operator`-only method did not merely return; it hired.
    let _hired: HireResult = admitted.json();

    let refused = child.env("HOME", stranger.path()).run();
    assert_eq!(
        refused.code(),
        exit::REFUSED,
        "control: with no credential under $HOME the same child presents nothing\n{}",
        refused.output()
    );
    assert_eq!(
        refused.json_error().code,
        ErrorCode::Unauthorized.code(),
        "and the refusal is the authorisation one\n{}",
        refused.output()
    );

    running.stop();
}
