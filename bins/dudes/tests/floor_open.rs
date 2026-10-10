//! `dudes floor open` mints the operator bootstrap session over the operator door, prints
//! the one URL, discloses the bearer nowhere else, and refuses every start that is not ready.
//!
//! # The four refusals, and why they are three messages rather than three probes
//!
//! It refuses false readiness when the daemon, the gateway or the assets are missing. Two
//! of those three are answered on the wire and the third is answered before the wire exists:
//!
//! - **No daemon** — nothing is listening, so the CLI never gets to ask. `exit::UNREACHABLE`, with
//!   the socket path in the message.
//! - **No gateway** — the daemon runs with `settings.floor` unset, so `floor.session.issue`
//!   answers an error naming the Floor. `exit::OTHER`.
//! - **No assets** — `duded::boot` validates the whole asset tree *before* the listener binds and
//!   returns `Err` when it cannot, so a daemon with a broken tree never starts and the CLI meets
//!   the first case. That is asserted here from both ends: boot fails naming the manifest, and the
//!   CLI against that runtime exits `UNREACHABLE` having printed no URL. There is deliberately no
//!   loopback readiness probe — it would assert a state the daemon cannot produce.
//!
//! The command opens no browser on any platform: it prints the link and says where to paste it.
//!
//! # The leak assertion and its control
//!
//! `tests/token.rs`'s idiom, over the value this verb exists to print. An absence proves nothing on
//! its own — a capture that saw no output would satisfy it — so every arm below asserts the bearer
//! is absent *from a capture that is simultaneously shown to carry `base_url`*, which is the other
//! daemon-derived value on the same stream. `RUST_LOG=trace` is set on every run: this binary links
//! no subscriber at all (`src/main.rs`), and setting it anyway is what makes that a measurement
//! rather than a claim.
//!
//! What the bearer is *permitted* one of is stdout: exactly one disclosure is admitted —
//! the URL printed once by the requesting CLI — so the assertion is an exact count of one on stdout and
//! zero on stderr, not an absence. A count catches the regression an absence check cannot: a debug
//! line that formats the whole URL a second time.
#![cfg(unix)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use duded::settings::{FloorSettings, Settings};
use dudes::exit;
use dudes_proto::rpc::floor::{SESSION_BOOTSTRAP_LEN, SessionIssueResult};

mod common;

use common::{Cli, Fixture, OpenRunner, Run, SharedHost, seams, start};

/// The asset tree `bins/duded/tests/floor_session.rs` serves the gateway from.
///
/// Reached across the package boundary rather than copied: two release fixtures would be two things
/// to keep in step with the manifest's pinned daemon version, and this suite asserts nothing about
/// the bytes — only that a gateway which has them answers, and one without them does not start.
const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../duded/tests/fixtures/floor-release-v3"
);

/// The repository root, for the two cross-language pins below.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root is readable")
}

/// Settings whose daemon serves the Floor from the shared fixture on an ephemeral port.
fn with_floor(fixture: &Fixture) -> Settings {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: PathBuf::from(FLOOR_FIXTURE),
        port: Some(0),
    });
    settings
}

/// `dudes floor open` against this daemon, at the loudest this binary can be run.
fn open(settings: &Settings, machine: bool) -> Run {
    let mut args = vec![OsString::from("floor"), OsString::from("open")];
    if machine {
        args.insert(0, OsString::from("--json"));
    }
    Cli::new(&settings.socket(), &settings.runtime_dir)
        .env("RUST_LOG", "trace")
        .args(args)
        .run()
}

/// How many times `needle` occurs in `text`.
fn occurrences(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

// ---------------------------------------------------------------------------------------------
// The URL
// ---------------------------------------------------------------------------------------------

/// **The two constants this CLI copies still match the sources that own them.**
///
/// `dudes` links neither `duded` in production nor TypeScript at all, so the route and the fragment
/// prefix are copies. This is the assertion that makes a copy safe: the gateway's own `BASE_PATH`
/// is read through the dev-dependency, and the browser app's prefix is read out of its source.
/// Either owner moving without this crate moving is red here.
#[test]
fn the_route_and_the_fragment_match_their_owners() {
    assert_eq!(
        dudes::floor::BASE_PATH,
        duded::floor_gateway::BASE_PATH,
        "the CLI's copy of the gateway's public root has drifted"
    );

    let session = repo_root().join("floor/src/session.ts");
    let source = std::fs::read_to_string(&session)
        .unwrap_or_else(|error| panic!("reading {}: {error}", session.display()));
    // The literal, as the app writes it. A prefix the app does not slice on would leave the browser
    // with no bearer and no error, which is the failure this pin exists to make loud.
    let quoted = format!("'{}'", dudes::floor::FRAGMENT_PREFIX);
    assert!(
        source.contains(&quoted),
        "{} does not contain {quoted}",
        session.display()
    );
}

/// **A live daemon's URL is the one the browser app can consume.**
///
/// Composed from a real `floor.session.issue` answer rather than a fixture: the origin is whatever
/// port the listener took, so this is the assertion that the CLI joins the daemon's own value to the
/// route rather than to a constant it invented.
#[test]
fn the_printed_url_is_the_live_daemons_origin_plus_the_route_and_fragment() {
    let fixture = Fixture::new();
    let settings = with_floor(&fixture);
    let host = SharedHost::new();
    let runner = Arc::new(OpenRunner::default());
    let _daemon = start(&settings, host, runner);

    let machine = open(&settings, true);
    assert_eq!(machine.code(), exit::SUCCESS, "{}", machine.output());
    let issued: SessionIssueResult = machine.json();

    let human = open(&settings, false);
    assert_eq!(human.code(), exit::SUCCESS, "{}", human.output());
    let url = human
        .stdout
        .lines()
        .next()
        .expect("the URL is the first line");

    assert!(
        url.starts_with(&format!("{}/floor/#bootstrap=", issued.base_url)),
        "{url} is not {}'s app route with a fragment",
        issued.base_url
    );
    let bearer = url
        .rsplit("#bootstrap=")
        .next()
        .expect("the fragment carries a value");
    assert_eq!(
        bearer.len(),
        SESSION_BOOTSTRAP_LEN,
        "the fragment carries {bearer:?}, not a 43-character bearer"
    );
    // Two runs, two bearers: single-use means the second call cannot reissue the first.
    assert_ne!(
        bearer, issued.bootstrap,
        "two issue calls returned the same bearer"
    );
}

// ---------------------------------------------------------------------------------------------
// The one disclosure
// ---------------------------------------------------------------------------------------------

/// **The bearer is written once, on stdout, and reaches no other sink — in either mode.**
///
/// The firing control for the print-the-bearer-once guarantee. `base_url` is the control value on the same capture: it
/// is daemon-derived, it is on the same stream, and a capture that had stopped working would lose
/// it too.
#[test]
fn the_bearer_is_printed_once_on_stdout_and_nowhere_else() {
    let fixture = Fixture::new();
    let settings = with_floor(&fixture);
    let host = SharedHost::new();
    let runner = Arc::new(OpenRunner::default());
    let _daemon = start(&settings, host, runner);

    for machine in [false, true] {
        let run = open(&settings, machine);
        assert_eq!(run.code(), exit::SUCCESS, "{}", run.output());

        let issued: SessionIssueResult = if machine {
            run.json()
        } else {
            // The human mode prints no JSON, so the bearer is read back out of the one line that
            // carries it — which is also the only place this test will then tolerate it.
            let url = run.stdout.lines().next().expect("a URL line");
            let bearer = url.rsplit("#bootstrap=").next().expect("a fragment");
            serde_json::from_value(serde_json::json!({
                "base_url": url
                    .split("/floor/")
                    .next()
                    .expect("an origin")
                    .to_owned(),
                "bootstrap": bearer,
                "expires_in_s": 60,
            }))
            .expect("the printed URL carries a contract-shaped session")
        };

        assert_eq!(
            occurrences(&run.stdout, &issued.bootstrap),
            1,
            "machine={machine}: the bearer is on stdout {} times, not once:\n{}",
            occurrences(&run.stdout, &issued.bootstrap),
            run.stdout
        );
        assert_eq!(
            occurrences(&run.stderr, &issued.bootstrap),
            0,
            "machine={machine}: the bearer reached stderr:\n{}",
            run.stderr
        );
        // The positive control, on the same capture that just proved an absence.
        assert!(
            run.stdout.contains(&issued.base_url),
            "machine={machine}: the control value is absent, so the capture proves nothing:\n{}",
            run.stdout
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The refusals
// ---------------------------------------------------------------------------------------------

/// **Invariant 8: without the operator credential the call is refused, and nothing is printed.**
///
/// Both arms run against one daemon, because the pair is the assertion: the refusing arm alone
/// cannot tell a `Requirement` refusal from an unregistered method, and the succeeding arm alone cannot
/// tell a credential being presented from one not being needed.
#[test]
fn a_caller_that_presents_no_operator_credential_is_refused() {
    let fixture = Fixture::new();
    let settings = with_floor(&fixture);
    let host = SharedHost::new();
    let runner = Arc::new(OpenRunner::default());
    let _daemon = start(&settings, host, runner);

    let presenting = open(&settings, false);
    assert_eq!(
        presenting.code(),
        exit::SUCCESS,
        "the operator door: {}",
        presenting.output()
    );

    // Same socket, same registrations; only the directory the credential is read from differs.
    let refused = Cli::new(&settings.socket(), fixture.empty.path())
        .env("RUST_LOG", "trace")
        .args(["floor", "open", "--no-browser"])
        .run();
    assert_eq!(
        refused.code(),
        exit::REFUSED,
        "presenting nothing was not refused: {}",
        refused.output()
    );
    assert!(
        !refused.stdout.contains("#bootstrap="),
        "a refused call printed a URL:\n{}",
        refused.stdout
    );
}

/// **A daemon serving no Floor says so, and prints no URL.**
#[test]
fn a_daemon_without_a_gateway_names_the_floor() {
    let fixture = Fixture::new();
    // `Fixture::settings` leaves `floor` unset: a daemon with no `DUDES_FLOOR_ASSETS` and no
    // Floor installed beside it.
    let settings = fixture.settings();
    assert!(settings.floor.is_none(), "this arm needs no gateway");
    let host = SharedHost::new();
    let runner = Arc::new(OpenRunner::default());
    let _daemon = start(&settings, host, runner);

    let run = open(&settings, false);
    assert_eq!(run.code(), exit::OTHER, "{}", run.output());
    assert!(
        run.stderr.contains(
            "the Floor is not configured: install the built Floor as a `floor` \
                       directory beside duded"
        ),
        "the refusal does not name the Floor and what to set:\n{}",
        run.stderr
    );
    assert!(
        !run.stdout.contains("#bootstrap="),
        "a daemon with no gateway still produced a URL:\n{}",
        run.stdout
    );
}

/// **No daemon at all is unreachable, named by socket path, and prints no URL.**
#[test]
fn no_daemon_is_unreachable_and_names_the_socket() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    // Nothing is started: the socket path exists as a value and as no listener.
    let run = open(&settings, false);
    assert_eq!(run.code(), exit::UNREACHABLE, "{}", run.output());
    assert!(
        run.stderr
            .contains(&settings.socket().display().to_string()),
        "the message does not name the endpoint:\n{}",
        run.stderr
    );
    assert!(run.stdout.is_empty(), "stdout: {}", run.stdout);
}

/// **A Floor with no release manifest stops the daemon, and the CLI reports no ready Floor.**
///
/// The missing-assets case. The refusal is the daemon's and it happens before the listener binds, so
/// this asserts both ends of it rather than inventing a third CLI message for a state the CLI
/// cannot observe: `boot` fails naming the manifest, and a CLI run against that runtime exits
/// `UNREACHABLE` with no URL on stdout.
#[test]
fn a_floor_with_no_release_manifest_refuses_to_start_the_daemon() {
    let fixture = Fixture::new();
    let assets = tempfile::tempdir().expect("an asset directory");
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: assets.path().to_path_buf(),
        port: Some(0),
    });

    let failure = duded::boot(
        &settings,
        seams(SharedHost::new(), Arc::new(OpenRunner::default())),
    )
    .err()
    .expect("a Floor with no release manifest does not boot");
    let reported = format!("{failure:#}");
    assert!(
        reported.contains("release-manifest.json"),
        "the daemon's refusal does not name the manifest: {reported}"
    );

    let run = open(&settings, false);
    assert_eq!(run.code(), exit::UNREACHABLE, "{}", run.output());
    assert!(
        run.stdout.is_empty(),
        "a Floor that never started produced output:\n{}",
        run.stdout
    );
}

/// The launch files an earlier version wrote into `dir`.
fn launch_files(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("the runtime directory")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("floor-open-"))
        })
        .collect();
    found.sort();
    found
}

/// What the command prints besides the link.
const PASTE: &str = "Paste this link into any browser within 60 seconds. It works once; run dudes \
                     floor open for a new one.";

/// **The command prints the link on its own line, then where to paste it, and launches nothing.**
///
/// A launched opener leaves a launch file in the runtime directory and, over SSH, a line on
/// stderr saying why it did not run; neither appears, with or without an SSH session. The old
/// `--no-browser` flag is accepted and prints the same.
#[test]
fn the_command_prints_the_link_and_launches_nothing() {
    let fixture = Fixture::new();
    let settings = with_floor(&fixture);
    let _daemon = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let shape = |text: &str| -> Vec<String> {
        text.lines()
            .map(|line| line.split("#bootstrap=").next().unwrap_or(line).to_owned())
            .collect()
    };

    // The daemon admits four Floor sessions, so the runs below stay under that.
    let mut first: Option<Vec<String>> = None;
    for (ssh, flag) in [(true, false), (false, false), (true, true)] {
        let mut cli = Cli::new(&settings.socket(), &settings.runtime_dir);
        if ssh {
            cli = cli.env("SSH_CONNECTION", "10.0.0.1 22 10.0.0.2 22");
        }
        let mut args = vec!["floor", "open"];
        if flag {
            args.push("--no-browser");
        }
        let run = cli.args(args).run();
        assert_eq!(run.code(), exit::SUCCESS, "ssh={ssh}: {}", run.output());
        assert_eq!(run.stderr, "", "ssh={ssh} flag={flag}");
        let lines: Vec<&str> = run.stdout.lines().collect();
        assert_eq!(lines.len(), 2, "{}", run.output());
        assert!(
            lines[0].starts_with("http://127.0.0.1:") && lines[0].contains("/floor/#bootstrap="),
            "the link is not on its own first line: {}",
            run.output()
        );
        assert_eq!(lines[1], PASTE);
        assert_eq!(occurrences(&run.stdout, "#bootstrap="), 1);
        assert_eq!(launch_files(&settings.runtime_dir), Vec::<PathBuf>::new());
        let shown = shape(&run.stdout);
        assert_eq!(*first.get_or_insert_with(|| shown.clone()), shown);
    }
}

/// **`--json` needs no flag**: the contract type is what a script parses.
#[test]
fn json_prints_the_contract_type() {
    let fixture = Fixture::new();
    let settings = with_floor(&fixture);
    let _daemon = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let run = open(&settings, true);
    assert_eq!(run.code(), exit::SUCCESS, "{}", run.output());
    let issued: SessionIssueResult = run.json();
    assert_eq!(issued.expires_in_s, 60);
}
