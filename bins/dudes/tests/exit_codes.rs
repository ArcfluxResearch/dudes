//! The exit code differs for a refused call, an unreachable daemon and a success.
//!
//! # Three outcomes produced three different ways, in one test
//!
//! The requirement is a *difference*, so the three are asserted together and their pairwise
//! distinctness is asserted explicitly. A binary that returned `1` for everything would satisfy any
//! one of these arms read alone.
//!
//! # The contract behind these numbers, and the two different things asserted here
//!
//! All five values are a public contract, so this
//! file is where the numbers are pinned. Two assertions live here
//! and they catch different mutations. The *discrimination* — three outcomes produced three ways
//! come back as three different codes — is what a script branches on, and it reads the constants by
//! name. The *values* are asserted as literals, because a symbol compared against a symbol cannot
//! fail: swap two constants and every behavioural arm below stays green.
//!
//! **Only the numeric assertions are platform-independent, and nothing in this file gates them
//! out.** The arms that drive a real binary over a Unix socket carry their own `#[cfg(unix)]`; the
//! file does not, so the pins are compiled wherever this crate is. That removes a precondition and
//! claims no coverage: `bins/dudes` does not build for `x86_64-pc-windows-msvc` today — the library
//! reaches for a Unix socket unconditionally, so no test target in this crate is reached and
//! nothing here runs on that leg yet. When the crate builds there, these pins run there too,
//! without a further change in this file.

use dudes::exit;

#[cfg(unix)]
use std::collections::BTreeSet;
#[cfg(unix)]
use std::sync::Arc;

#[cfg(unix)]
mod common;

#[cfg(unix)]
use common::{Cli, Fixture, OpenRunner, SharedHost, start};

/// The five numbers, asserted as literals, because a symbol asserted against a symbol cannot fail.
///
/// Every other `exit::` comparison in this crate reads the constant by name on **both** sides, so
/// swapping two values leaves all of them green: the five codes stay distinct, `SUCCESS` is still
/// `0`, and every behavioural arm still matches. Only a literal on the right-hand side turns that
/// red, which is what pins the numbers.
#[test]
fn the_five_values_are_the_contract_numbers() {
    assert_eq!(exit::SUCCESS, 0);
    assert_eq!(exit::USAGE, 1);
    assert_eq!(exit::UNREACHABLE, 2);
    assert_eq!(exit::REFUSED, 3);
    assert_eq!(exit::OTHER, 4);
}

/// The numbers of the `EXIT STATUS` rows in `text`, in the order they are printed.
///
/// A row is four spaces, one digit, then two spaces and its text; a wrapped row's continuation is
/// indented past that and is not a row. With no block at all this returns empty, which the caller
/// reports as a mismatch against [`exit::ALL`] rather than as a panic.
fn exit_status_rows(text: &str) -> Vec<u8> {
    let Some((_, block)) = text.split_once("EXIT STATUS\n") else {
        return Vec::new();
    };
    block
        .lines()
        .take_while(|line| line.is_empty() || line.starts_with(' '))
        .filter_map(|line| {
            let rest = line.strip_prefix("    ")?;
            let digit = rest.chars().next()?.to_digit(10)?;
            if !rest[1..].starts_with("  ") {
                return None;
            }
            u8::try_from(digit).ok()
        })
        .collect()
}

/// Control on [`exit_status_rows`], and it runs on every platform.
///
/// Without it the help assertion below could pass by finding nothing on both sides, or fail for a
/// reason that has nothing to do with the codes. So the parser is shown to find rows, to skip a
/// wrapped row's continuation, and to return empty rather than inventing a row when the block is
/// absent.
#[test]
fn exit_status_rows_reads_rows_and_not_their_continuations() {
    let block = "\
USAGE
    dudes [--json] <command> [options]

EXIT STATUS
    0  ran the method
    2  no conversation happened: the socket was absent, unconnectable, or closed
       without answering
    4  anything else
";
    assert_eq!(exit_status_rows(block), vec![0, 2, 4]);
    assert!(exit_status_rows("nothing here\n").is_empty());
}

#[cfg(unix)]
#[test]
fn success_refusal_unreachability_and_usage_are_four_different_exit_codes() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let mut running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let socket = settings.socket();

    // A success: `system.ping` admits every role, so it needs nothing presented.
    let success = Cli::new(&socket, fixture.empty.path()).args(["ping"]).run();

    // A refusal: an `Operator` row with nothing presented. The daemon answered — the connection was
    // made, the frame went out, an error came back — which is what makes this different from the
    // arm below.
    let refused = Cli::new(&socket, fixture.empty.path())
        .args(common::hire_args(fixture.state.path()))
        .run();

    // Unreachable: a socket path with nothing behind it. The daemon above is still running, so this
    // is not "no daemon anywhere" — it is "not at that path", which is the case a retry after
    // starting one would not fix and a retry after fixing the path would.
    let unreachable = Cli::new(
        &fixture.empty.path().join("nothing.sock"),
        fixture.empty.path(),
    )
    .args(["ping"])
    .run();

    // Usage: nothing was sent at all.
    let usage = Cli::new(&socket, &settings.runtime_dir)
        .args(["hire", "--name"])
        .run();

    assert_eq!(
        success.code(),
        exit::SUCCESS,
        "a served call\n{}",
        success.output()
    );
    assert_eq!(
        refused.code(),
        exit::REFUSED,
        "a refused call\n{}",
        refused.output()
    );
    assert_eq!(
        unreachable.code(),
        exit::UNREACHABLE,
        "an unreachable daemon\n{}",
        unreachable.output()
    );
    assert_eq!(
        usage.code(),
        exit::USAGE,
        "arguments that never reached a socket\n{}",
        usage.output()
    );

    // The property that matters: a script can tell them apart.
    let codes: BTreeSet<u8> = [
        success.code(),
        refused.code(),
        unreachable.code(),
        usage.code(),
    ]
    .into_iter()
    .collect();
    assert_eq!(codes.len(), 4, "four outcomes, four codes: {codes:?}");

    running.stop();
}

/// The two failures a loop has to tell apart are told apart by what they *say*, too — because an
/// exit code with no message is a diagnosis nobody can act on.
///
/// The unreachable message names the socket path. That is not decoration: `token.rs` uses this same
/// line as its positive control, because it proves an environment-derived value *does* reach the
/// output — which is what makes "the token does not" an assertion rather than an artefact of a
/// capture that sees nothing.
#[cfg(unix)]
#[test]
fn an_unreachable_daemon_names_the_path_and_a_refusal_names_the_rule() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let mut running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );

    let missing = fixture.empty.path().join("nothing.sock");
    let unreachable = Cli::new(&missing, fixture.empty.path())
        .args(["ping"])
        .run();
    assert!(
        unreachable
            .output()
            .contains(&missing.display().to_string()),
        "the message names where it looked: {}",
        unreachable.output()
    );

    let refused = Cli::new(&settings.socket(), fixture.empty.path())
        .args(["why", "--agent", "01ARZ3NDEKTSV4RRFFQ69G5FAV"])
        .run();
    assert!(
        refused.output().contains("-32001"),
        "the refusal carries the code a script branches on: {}",
        refused.output()
    );
    assert!(
        !refused.output().contains(&missing.display().to_string()),
        "control: the two messages are different messages"
    );

    running.stop();
}

/// `--help` prints the usage on stdout; a bad command prints the mistake and a pointer to `--help`
/// on stderr, not the whole usage text.
///
/// A lone `--help` or `-h` is the one help request that exits [`exit::SUCCESS`], so a new user's
/// install check passes. A help token anywhere else, such as `hire --help`, still exits
/// [`exit::USAGE`] (`tests/success_means_sent.rs` holds that over every other argv). The text goes
/// to stdout with no error line above it. Without the help arm a binary that answered a help
/// request the way it answers a typo would pass; without the unknown arm, one that never said what
/// it had failed to understand would.
#[cfg(unix)]
#[test]
fn help_and_an_unknown_command_differ_by_stream_not_by_code() {
    let help = Cli::bare().args(["--help"]).run();
    assert_eq!(help.code(), exit::SUCCESS, "{}", help.output());
    assert!(help.stdout.contains("USAGE"), "{}", help.stdout);
    assert!(help.stdout.contains("Chat is always on"), "{}", help.stdout);
    assert!(
        help.stderr.is_empty(),
        "a help request is not an error and writes no error line: {}",
        help.stderr
    );
    let short = Cli::bare().args(["-h"]).run();
    assert_eq!(short.code(), exit::SUCCESS, "{}", short.output());
    assert_eq!(short.stdout, help.stdout);

    let nested = Cli::bare().args(["hire", "--help"]).run();
    assert_eq!(nested.code(), exit::USAGE, "{}", nested.output());
    assert_eq!(nested.stdout, help.stdout);

    let unknown = Cli::bare().args(["nosuch"]).run();
    assert_eq!(unknown.code(), exit::USAGE, "{}", unknown.output());
    assert!(
        unknown.stderr.contains("nosuch"),
        "the message names what it did not understand: {}",
        unknown.stderr
    );
    assert_eq!(
        unknown.stderr,
        "dudes: unknown command `nosuch`\nRun `dudes --help` for every command and its options.\n",
        "the mistake and where the usage is, and nothing else"
    );
}

/// `--version` and `-V` print `dudes <version>` on stdout and exit [`exit::SUCCESS`], so an install
/// check does not need a running daemon.
#[cfg(unix)]
#[test]
fn version_prints_the_workspace_version_and_succeeds() {
    for flag in ["--version", "-V"] {
        let version = Cli::bare().args([flag]).run();
        assert_eq!(version.code(), exit::SUCCESS, "{}", version.output());
        assert_eq!(
            version.stdout,
            format!("dudes {}\n", env!("CARGO_PKG_VERSION"))
        );
        assert!(version.stderr.is_empty(), "{}", version.stderr);
    }
}

/// The operator is told the same numbers the constants hold, in the same order.
///
/// `EXIT STATUS` in the usage text is the only copy of this contract an operator ever reads, and it
/// is a hand-written string — nothing generates it from [`exit::ALL`]. So it can be renumbered on
/// its own, one row at a time. A reader who renumbers
/// one has to remember the other; this makes the numeric half of that mechanical, by reading the block out of the **real binary's** `--help` and comparing
/// the numbers it finds against `exit::ALL`. The wording is deliberately not asserted: it is free
/// to improve, and the numbers are not.
#[cfg(unix)]
#[test]
fn the_help_text_lists_the_same_codes_in_the_same_order() {
    let help = Cli::bare().args(["--help"]).run();
    assert_eq!(help.code(), exit::SUCCESS, "{}", help.output());

    let rows = exit_status_rows(&help.stdout);
    assert_eq!(
        rows.as_slice(),
        exit::ALL.as_slice(),
        "EXIT STATUS lists {rows:?}, the constants are {:?}\n{}",
        exit::ALL,
        help.stdout
    );
}

/// A deliberate refusal by `uninstall` is exit 3, as a literal: an agent's token is one.
#[cfg(unix)]
#[test]
fn uninstall_refused_for_an_agents_token_exits_three() {
    let token = "t".repeat(dudes_proto::rpc::hook::HOOK_TOKEN_LEN);
    let run = Cli::bare()
        .env("DUDES_TOKEN", token)
        .args(["uninstall", "--yes"])
        .run();
    assert_eq!(run.code(), 3, "{}", run.output());
}

/// A deliberate refusal by `upgrade` is exit 3, as a literal: a copy `install.sh` did not put
/// there is one, and it is refused before any network request.
#[cfg(unix)]
#[test]
fn upgrade_refused_for_a_build_that_install_sh_did_not_make_exits_three() {
    let run = Cli::bare()
        .env("DUDES_STATE_DIR", "/nonexistent/state")
        .args(["upgrade", "--check"])
        .run();
    assert_eq!(run.code(), 3, "{}", run.output());
    assert!(
        run.stderr.starts_with("dudes: this copy of dudes ("),
        "{}",
        run.stderr
    );
}
