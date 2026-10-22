//! The daemon reaches its children through the seams, and creates none itself.
//!
//! # The daemon reaches the host through the trait
//!
//! Invariant 10's daemon-exit half is a **caller** duty, and the code
//! alone cannot establish that the daemon reaches the host through the trait. A daemon holding a
//! concrete `RealHost` and calling the inherent method would satisfy invariant 10 while making the
//! trait change pointless. This file is what turns that premise into a fact, in two arms:
//!
//! - **Compile.** [`child_launch_path`] is generic over `H: ProcessHost` and calls the three methods
//!   the executor calls. It stops compiling with `E0599` the moment one of them leaves the trait —
//!   the same shape `dudes-supervisor`'s `shutdown_seam.rs` uses.
//! - **Scan.** No file under `src/` but `main.rs` names a concrete child owner in code.
//!
//! # Where the child-creation gate really is
//!
//! The gate for the rule that every agent child is created by the host crate and by nothing else is
//! `bins/duded/clippy.toml`'s `disallowed-types`— not this scan. A planted
//! `std::process::Command` anywhere under `src/` **compiles** and is denied by
//! `cargo clippy -p duded --all-targets -- -D warnings`, which is a clean single-gate red.
//!
//! A planted `tokio::process::Command` is the *weaker*
//! control: this workspace links no runtime, so that plant does not compile and breaks the build
//! rather than reddening a gate. Both were run; the `std` one is what stands.
//!
//! The scan below is the third mechanism, and it is the cheapest: it catches the token in a source
//! file at test time rather than at lint time, which is what makes a plant visible in a failing
//! test name. The **complement** to all three is the `duded` row in `cargo xtask layering`, which
//! bans *linking* a runtime that would reap the host's children behind its back — a
//! row can ban a crate and cannot ban a type, and `disallowed-types` can ban a type and cannot ban
//! a crate. Neither expresses the other, and neither reaches every case.

use std::io;
use std::path::{Path, PathBuf};

use dudes_providers::adapter::ManagedInstall;
use dudes_providers::launch::LaunchSpec;
use dudes_supervisor::host::{AdoptionKey, KillOutcome, Liveness, ProcessHost, SpawnResult};

/// The daemon's child-launch path, reduced to the calls the executor makes.
///
/// Generic over the trait and not over a concrete host, because the claim is about the seam. This
/// is the arm that fails to compile if any of these three becomes inherent on a concrete type
/// instead of declared on `ProcessHost`.
fn child_launch_path<H: ProcessHost>(
    host: &mut H,
    spec: &LaunchSpec,
    install: &ManagedInstall,
) -> io::Result<(AdoptionKey, Liveness, KillOutcome)> {
    let key = match host.spawn(spec, install) {
        SpawnResult::Spawned(key) => key,
        SpawnResult::NotLaunched { reason } | SpawnResult::CleanupUnconfirmed { reason } => {
            return Err(reason);
        }
    };
    let liveness = host.alive(&key);
    let outcome = host.kill(&key);
    host.shutdown()?;
    Ok((key, liveness, outcome))
}

/// The concrete child owners. Named in code by `main.rs` and by nothing else under `src/`.
///
/// `OneShot` is on the list beside `RealHost` for the same reason it is in
/// `dudes-host`: it is the *other* class of child this process creates, and a daemon that named it
/// directly would be reaching past `PredicateRunner` exactly as one naming `RealHost` reaches past
/// `ProcessHost`.
const CONCRETE_OWNERS: &[&str] = &["RealHost", "dudes_host", "OneShot", "dudes_pty"];

/// The file that may name them.
const ALLOWED: &str = "main.rs";

/// The one process-host leaf `boot.rs` may inject into the transport.
///
/// This function disconnects a named pipe and owns no child. Naming the exact path keeps the
/// exception smaller than the `dudes_host` crate: another concrete owner or host method in
/// `boot.rs` is still caught by the scan below.
const BOOT_TRANSPORT_CAPABILITY: &str = "dudes_host::disconnect_named_pipe";

/// The one process-host leaf `floor_peer.rs` may name.
///
/// `tcp_peer` reads the socket tables to find who owns a loopback connection and creates nothing:
/// no child, no signal. Naming the exact path keeps the exception smaller than the crate.
const FLOOR_PEER_CAPABILITY: &str = "dudes_host::tcp_peer";

/// Ways to reach a process without going through `dudes-host`. Allowed in **no** file, `main.rs`
/// included: the ban has no exception, because an allow-listed module is a permanent
/// blind spot exactly its own size.
const PROCESS_ROUTES: &[&str] = &["process::Command", "tokio::process", "pre_exec"];

/// The way the clippy gate could be answered from inside `src/`.
///
/// `disallowed-types` is a lint, and a lint can be allowed at a statement. The rule has no module
/// allow-list precisely so that an exemption cannot hide in one — so this scan makes sure it does
/// not hide behind an attribute either. (`tests/restart.rs` carries exactly one, spelled at the
/// statement that needs it, with a reason: it is a harness starting the daemon, not the daemon
/// creating a child.)
const GATE_ESCAPES: &[&str] = &["disallowed_types"];

/// The permissive authorisers. Behind `dudes-rpc`'s `testing` feature, which this package
/// does not enable — so this is a check that a *compile-time* fact stayed true, not the fact itself.
const PERMISSIVE_AUTHORIZERS: &[&str] = &["AdmitAll", "RefuseAll"];

/// This package's `src/` directory, found from the manifest so the scan works under any runner.
fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// The floor under the number of files every scan below must see.
///
/// **On `sources()` rather than on one caller**, which is where it was: two of the three scans had
/// no corpus guard of their own, so a `src/` that had moved would have made them silently green.
/// The guard belongs to the walk, so every caller inherits it.
const MIN_SOURCES: usize = 8;

/// Every `.rs` file under `src/`, at any depth.
fn sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("src/ is where it was") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(&src_dir(), &mut found);
    found.sort();
    assert!(
        found.len() >= MIN_SOURCES,
        "scanned only {} files under {}; is src/ where it was?",
        found.len(),
        src_dir().display()
    );
    found
}

/// The tokens from `tokens` that appear in `text`'s **code** lines.
///
/// A line counts as prose when the first thing on it is `//`, which covers `//!` module docs, `///`
/// item docs and ordinary comments. That distinction is the whole reason the scan works here: every
/// module in this package has to be able to *discuss* `RealHost` and `std::process::Command` at
/// length in order to explain why it does not use them, and a scan that matched prose would either
/// forbid the explanation or be switched off.
fn found_in_code<'t>(text: &str, tokens: &[&'t str]) -> Vec<&'t str> {
    let code: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect();
    tokens
        .iter()
        .copied()
        .filter(|token| code.iter().any(|line| line.contains(token)))
        .collect()
}

/// The scan arm for the host seam: only `main.rs` names a concrete child owner in code.
#[test]
fn only_main_names_a_concrete_child_owner() {
    let sources = sources();
    let mut offenders = Vec::new();
    let mut allowed_names = Vec::new();
    for path in &sources {
        let text = std::fs::read_to_string(path).expect("a source file");
        let name = path.file_name().expect("a file name").to_string_lossy();
        let inspected = if name == "boot.rs" {
            let code = found_in_code(&text, &[BOOT_TRANSPORT_CAPABILITY]);
            assert_eq!(
                code,
                vec![BOOT_TRANSPORT_CAPABILITY],
                "boot.rs must name the injected named-pipe disconnect leaf"
            );
            text.replace(BOOT_TRANSPORT_CAPABILITY, "disconnect_named_pipe")
        } else if name == "floor_peer.rs" {
            let code = found_in_code(&text, &[FLOOR_PEER_CAPABILITY]);
            assert_eq!(
                code,
                vec![FLOOR_PEER_CAPABILITY],
                "floor_peer.rs must name the injected socket-owner leaf"
            );
            text.replace(FLOOR_PEER_CAPABILITY, "tcp_peer")
        } else {
            text
        };
        let found = found_in_code(&inspected, CONCRETE_OWNERS);
        if found.is_empty() {
            continue;
        }
        if name == ALLOWED {
            allowed_names.extend(found);
        } else {
            offenders.push(format!("{name}: {found:?}"));
        }
    }

    assert!(
        offenders.is_empty(),
        "only {ALLOWED} may name a concrete child owner in code; found {offenders:?}"
    );
    // The control against the scan silently covering nothing: `main.rs` really does name them, so a
    // clean result elsewhere is a result and not an empty walk.
    assert!(
        !allowed_names.is_empty(),
        "control: {ALLOWED} is expected to name the concrete owners it constructs"
    );
}

/// The scan arm for child creation: no file under `src/` reaches a process by any route but the host crate, and
/// none of them answers the clippy gate with an attribute either.
#[test]
fn no_file_reaches_a_process_directly() {
    let mut offenders = Vec::new();
    for path in sources() {
        let text = std::fs::read_to_string(&path).expect("a source file");
        // Both lists, and the second is what stops the first being answerable: `disallowed-types`
        // is a lint, and a lint can be allowed at a statement.
        let mut found = found_in_code(&text, PROCESS_ROUTES);
        found.extend(found_in_code(&text, GATE_ESCAPES));
        if !found.is_empty() {
            offenders.push(format!(
                "{}: {found:?}",
                path.file_name().expect("a file name").to_string_lossy()
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "every child in this process is created by dudes-host; found {offenders:?}"
    );
}

/// The daemon names no permissive authorizer.
///
/// # This scan is the gate here, and the compiler is not
///
/// `AdmitAll` and `RefuseAll` live behind `dudes-rpc`'s `testing` feature, which `bins/duded` does
/// not enable — so the compiler refuses them in the **shipped binary**, and only there. A
/// dev-dependency's features unify across a workspace build, and `bins/dudes/Cargo.toml`
/// dev-depends on `dudes-rpc` with `testing` *and* on `duded`. Measured, by planting
/// `pub use dudes_rpc::testing::AdmitAll as _Probe;` in `bins/duded/src/lib.rs`:
///
/// ```text
/// cargo build -p duded           → error[E0432]: unresolved import `dudes_rpc::testing`
/// cargo nextest run --workspace  → compiles clean
/// ```
///
/// **This test runs in the second graph**, where the gate is lifted — so what holds here is the
/// scan below and nothing else. The scan is not a reminder of a compiler gate: in the graph it
/// executes in, there is no such gate.
///
/// # Why it stays a scan for these names and is not retargeted at `PeerResolver` impls
///
/// The daemon has more than one: the host-backed process resolver and `classify::NoWalk`'s
/// unsupported-target fallback are both `PeerResolver`s in `src/`. A scan for that trait would
/// reject the safe production relation, so the needles stay the permissive authorisers, whose
/// count in `src/` is and should remain zero.
#[test]
fn no_permissive_authorizer_is_named() {
    for path in sources() {
        let text = std::fs::read_to_string(&path).expect("a source file");
        let found = found_in_code(&text, PERMISSIVE_AUTHORIZERS);
        assert!(
            found.is_empty(),
            "{} names {found:?}; the daemon's authorizer is the classifier `boot` builds, and \
             no source of this package may name a permissive one — this scan is what holds that \
             in the workspace test graph, where feature unification lifts the compiler's gate",
            path.file_name().expect("a file name").to_string_lossy()
        );
    }
}

/// The scans can fail, in the shapes they exist to catch — and do not fire on prose.
///
/// Without this, a scan whose tokens were misspelled, or which read the wrong lines, would report
/// the same clean answer for ever.
#[test]
fn the_scans_catch_a_planted_token_and_spare_prose() {
    for token in CONCRETE_OWNERS {
        let planted = format!("    let host = {token}::new();\n");
        assert_eq!(
            found_in_code(&planted, CONCRETE_OWNERS),
            vec![*token],
            "the scan must catch a planted `{token}`"
        );
        let prose = format!("    // a comment explaining why {token} is not used here\n");
        assert!(
            found_in_code(&prose, CONCRETE_OWNERS).is_empty(),
            "and must not fire on prose about `{token}`"
        );
    }
    for token in PROCESS_ROUTES {
        let planted = format!("    let mut command = std::{token}::new(\"true\");\n");
        assert!(
            found_in_code(&planted, PROCESS_ROUTES).contains(token),
            "the scan must catch a planted `{token}`"
        );
        let prose = format!("    // this module explains at length why it does not use {token}\n");
        assert!(
            found_in_code(&prose, PROCESS_ROUTES).is_empty(),
            "and must not fire on prose about `{token}`"
        );
    }
    for token in PERMISSIVE_AUTHORIZERS {
        let planted = format!("    authorizer: Box::new({token}),\n");
        assert!(
            found_in_code(&planted, PERMISSIVE_AUTHORIZERS).contains(token),
            "the scan must catch a planted `{token}`"
        );
        let prose = format!("    /// `{token}` is behind a feature this package does not enable\n");
        assert!(
            found_in_code(&prose, PERMISSIVE_AUTHORIZERS).is_empty(),
            "and must not fire on prose about `{token}`"
        );
    }
    for token in GATE_ESCAPES {
        let planted = format!("    #[allow(clippy::{token})]\n");
        assert!(
            found_in_code(&planted, GATE_ESCAPES).contains(token),
            "the scan must catch a planted `{token}` attribute"
        );
        let prose = format!("    // the gate could be answered with clippy::{token}, and is not\n");
        assert!(
            found_in_code(&prose, GATE_ESCAPES).is_empty(),
            "and must not fire on prose about `{token}`"
        );
    }

    // Tripwires: a token deleted from a list makes the list shorter and the loops above simply do
    // less, still green. These are what notice that.
    assert_eq!(CONCRETE_OWNERS.len(), 4);
    assert_eq!(PROCESS_ROUTES.len(), 3);
    assert_eq!(PERMISSIVE_AUTHORIZERS.len(), 2);
    assert_eq!(GATE_ESCAPES.len(), 1);
}

/// The compile arm is exercised, so it is not dead code the linker could drop.
#[test]
fn the_generic_child_launch_path_drives_the_trait() {
    use dudes_supervisor::testing::FakeHost;

    let mut host = FakeHost::wall_clock_anchored();
    let spec = LaunchSpec {
        argv: vec![std::ffi::OsString::from("/usr/bin/true")],
        env: Vec::new(),
        cwd: PathBuf::from("/"),
        stdin: dudes_providers::launch::LaunchInput::Null,
    };
    let (key, liveness, outcome) =
        child_launch_path(&mut host, &spec, &ManagedInstall::default()).expect("the fake spawns");
    assert_eq!(
        liveness,
        Liveness::Alive,
        "control: the child was alive when the path probed it"
    );
    assert!(
        matches!(outcome, KillOutcome::Signalled),
        "and the kill was authorised: {outcome:?}"
    );
    assert_eq!(host.shutdowns, 1, "and the shutdown reached the seam");
    assert_eq!(
        host.alive(&key),
        Liveness::Gone,
        "the fake models the sweep, so the child reads gone afterwards"
    );
}
