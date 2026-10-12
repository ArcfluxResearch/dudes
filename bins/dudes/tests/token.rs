//! With `DUDES_TOKEN` and `DUDES_SOCK` in the environment, the CLI reads both, and the token
//! appears in no argv element and no log line.
//!
//! # The two halves, and why the second needs a control
//!
//! **Read**: `Environment::resolve` returns the token, asserted in-process against a lookup this
//! test controls. **Not leaked**: the real binary is run with the token in its environment across a
//! success, a refusal and an unreachable daemon, and a distinctive sentinel appears in neither
//! stream.
//!
//! An absence proves nothing on its own — a capture that saw *no* output would satisfy it — so every
//! leak assertion here sits beside a **positive control on the same capture**: the socket path,
//! which is also an environment-derived value, and which the unreachable message does print. That is
//! the idiom `crates/dudes-supervisor/tests/agent_token.rs` uses, and it is the assertion that
//! actually fails if the capture stops working.
//!
//! # The token's admitted use
//!
//! `agent.ask` is the one Agent-only row this binary carries. Its process-level test resolves the
//! CLI as the token's agent, requires the handler to observe that role and exact text, and leaves
//! the operator credential absent. That is the stronger presentation claim.
//!
//! # Four outcomes and seven commands, because the first alone is not enough
//!
//! `ping`, `why`, an unreachable `ping` and a usage failure are four **outcomes**, not four
//! commands. A leak on the shared success path reddens them, but a leak gated to `hire` alone
//! would stay green, because `hire`, a successful `goal` and `tail` never run with `DUDES_TOKEN`
//! set there. So the per-command loop below is a second axis over the same property.
//!
//! There is also a **structural** defence: `render_answer` does not take the [`Environment`] at all, so no renderer can reach the
//! token without a signature change. The only place a leak could be written is `dispatch`.
//!
#![cfg(unix)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::{Arc, mpsc};

use dudes::Environment;
use dudes::exit;
use dudes_proto::rpc::Empty;
use dudes_proto::rpc::cred::{OPERATOR_CREDENTIAL_LEN, OperatorCredential};
use dudes_proto::rpc::hook::HOOK_TOKEN_LEN;
use dudes_rpc::Dispatcher;
use dudes_rpc::auth::{PeerClassifier, PeerProvenance, PeerRole, Requirement};
use dudes_rpc::testing::{FakeMintedTokens, FakeResolver, TestServer};

mod common;

use common::{Cli, Fixture, OpenRunner, SharedHost, start};

/// A token that is well formed and unmistakable in a haystack: 43 base64url characters that no
/// other value in this repository spells.
///
/// The distinctiveness is what makes the absence assertions worth anything. A token of 43 `a`s
/// would be a substring of ordinary prose and could not be searched for.
const SENTINEL: &str = "zq7Lk3vWx9PbNmT2sRdYcFgHjKlQaZeUiOpAsDf";

fn sentinel() -> String {
    let mut text = SENTINEL.to_owned();
    // Padded rather than written out at full length, so the constant above stays readable and the
    // width is the contract's rather than a hand-counted literal.
    while text.len() < HOOK_TOKEN_LEN {
        text.push('X');
    }
    text.truncate(HOOK_TOKEN_LEN);
    text
}

fn ask_agent() -> dudes_types::AgentId {
    "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("an agent id")
}

fn start_ask_server(
    runtime_dir: std::path::PathBuf,
    token: &str,
) -> (TestServer, mpsc::Receiver<(PeerRole, String)>) {
    let agent = ask_agent();
    let credential = OperatorCredential::new("C".repeat(OPERATOR_CREDENTIAL_LEN))
        .expect("a well-formed unused credential");
    let classifier = PeerClassifier::new(
        credential,
        Arc::new(FakeMintedTokens::minted(&[(token, agent)])),
    );
    let (called, observed) = mpsc::channel();
    let mut dispatcher = Dispatcher::new(classifier);
    dispatcher.register_bounded_question(Requirement::Agent, move |call| {
        let params: dudes_proto::rpc::agent::AskParams =
            serde_json::from_value(call.params.cloned().expect("agent.ask carries params"))
                .expect("the contract's AskParams");
        called
            .send((*call.role, params.text))
            .expect("the observation receiver is alive");
        Ok(serde_json::to_value(Empty {}).expect("Empty encodes"))
    });
    let resolver = Arc::new(FakeResolver::new(PeerProvenance::Agent(agent)));
    let server = TestServer::start_with_resolver(
        runtime_dir,
        dispatcher,
        resolver as Arc<dyn dudes_rpc::auth::PeerResolver>,
    );
    (server, observed)
}

/// The read half, in process. Both arms: a token present is discovered, and a token absent is not
/// invented.
#[test]
fn the_cli_reads_the_token_and_the_socket_from_the_environment() {
    let token = sentinel();
    let pairs: BTreeMap<String, OsString> = [
        ("DUDES_SOCK".to_owned(), OsString::from("/tmp/some.sock")),
        ("DUDES_TOKEN".to_owned(), OsString::from(token.clone())),
    ]
    .into_iter()
    .collect();

    let resolved = Environment::resolve(|name| pairs.get(name).cloned());
    assert_eq!(
        resolved.socket(),
        Ok(std::path::Path::new("/tmp/some.sock")),
        "DUDES_SOCK is read"
    );
    assert_eq!(
        resolved
            .agent_token()
            .map(dudes_proto::rpc::hook::HookToken::expose),
        Some(token.as_str()),
        "and DUDES_TOKEN is read beside it"
    );

    let neither = Environment::resolve(|_| None);
    assert!(
        neither.agent_token().is_none(),
        "control: with nothing in the environment, nothing is discovered"
    );
}

/// `dudes ask` is the agent's door: it presents the inherited token, does not read the operator
/// credential, and carries no caller-supplied agent id.
///
/// The server resolves the real CLI process into A's ancestry column and uses the production
/// classifier over a token minted for A. Reaching the handler as `Agent(A)` therefore proves both
/// inputs were presented together; the empty credential directory proves the command did not
/// silently take the operator path. The handler reads the contract's own params and returns its
/// own result type, so success cannot be an unrelated method answering an untyped object.
#[test]
fn ask_presents_the_inherited_agent_token_without_an_operator_credential() {
    let agent = ask_agent();
    let token = sentinel();
    let root = tempfile::tempdir().expect("a temporary root");
    let (server, observed) = start_ask_server(root.path().join("run"), &token);

    let question = "which branch should I use?";
    let run = Cli::new(server.socket(), root.path().join("no-credential").as_path())
        .env("DUDES_TOKEN", &token)
        .args(["--json", "ask", "--text", question])
        .run();

    assert_eq!(run.code(), exit::SUCCESS, "{}", run.output());
    assert_eq!(run.json::<Empty>(), Empty {});
    assert_eq!(
        observed.recv().expect("the handler ran"),
        (PeerRole::Agent { agent }, question.to_owned()),
        "the inherited token and resolved peer select A; params cannot name anyone else"
    );
    assert!(
        !run.output().contains(&token) && !run.output().contains(&token[..12]),
        "the presented token stays out of both output streams: {}",
        run.output()
    );

    let absent = Cli::new(server.socket(), root.path().join("no-credential").as_path())
        .args(["--json", "ask", "--text", "no inherited token"])
        .run();
    assert_eq!(absent.code(), exit::REFUSED, "{}", absent.output());
    assert!(
        matches!(observed.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "an absent token is sent as an absent presentation and never reaches the handler"
    );
}

/// The leak half, over the real binary, on every path it has — with the positive control on the
/// same capture.
#[test]
fn the_token_reaches_neither_output_stream_on_any_path() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let mut running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let socket = settings.socket();
    let token = sentinel();

    let missing = fixture.empty.path().join("nothing.sock");
    let runs = [
        // A success.
        (
            "success",
            Cli::new(&socket, &settings.runtime_dir)
                .env("DUDES_TOKEN", &token)
                .args(["--json", "ping"])
                .run(),
            exit::SUCCESS,
        ),
        // A refusal.
        (
            "refusal",
            Cli::new(&socket, fixture.empty.path())
                .env("DUDES_TOKEN", &token)
                .args(["why", "--agent", "01ARZ3NDEKTSV4RRFFQ69G5FAV"])
                .run(),
            exit::REFUSED,
        ),
        // An unreachable daemon — and this is the run that carries the control.
        (
            "unreachable",
            Cli::new(&missing, fixture.empty.path())
                .env("DUDES_TOKEN", &token)
                .args(["ping"])
                .run(),
            exit::UNREACHABLE,
        ),
        // Bad arguments, which never open a socket at all.
        (
            "usage",
            Cli::new(&socket, &settings.runtime_dir)
                .env("DUDES_TOKEN", &token)
                .args(["goal", "--agent"])
                .run(),
            exit::USAGE,
        ),
    ];

    for (what, run, expected) in &runs {
        assert_eq!(
            run.code(),
            *expected,
            "{what} took the path it was supposed to\n{}",
            run.output()
        );
        assert!(
            !run.output().contains(&token),
            "{what}: the token reached the output\n{}",
            run.output()
        );
        // And no prefix of it either, which is what a truncated or partially redacted print would
        // leave behind.
        assert!(
            !run.output().contains(&token[..12]),
            "{what}: a prefix of the token reached the output\n{}",
            run.output()
        );
    }

    // --- the positive control, on one of the very captures above --------------------------------
    // The unreachable run prints the socket path, which is also an environment-derived value. So
    // this capture demonstrably *does* see environment-derived values reach the output — which is
    // what makes the four absences above assertions rather than an artefact of a capture that sees
    // nothing at all.
    let unreachable = &runs[2].1;
    assert!(
        unreachable
            .output()
            .contains(&missing.display().to_string()),
        "control: the capture sees an environment-derived value\n{}",
        unreachable.output()
    );

    running.stop();
}

/// The other axis: **every command this binary has**, each run to a success with the token set.
///
/// The test above varies the *outcome* and holds the command roughly fixed; this one varies the
/// *command* and holds the outcome at success. A leak written into one command's handler — a
/// mutation gated to `agent.hire` — is green under that test and red under
/// this one.
///
/// The control is the exit code on every row: each command must actually have run, or "no token in
/// the output" would be satisfied by a command that produced no output at all.
#[test]
fn the_token_reaches_no_output_of_any_command() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let mut running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let socket = settings.socket();
    let token = sentinel();
    let cli = || Cli::new(&socket, &settings.runtime_dir).env("DUDES_TOKEN", &token);
    let ask_root = tempfile::tempdir().expect("a temporary ask server root");
    let (ask_server, _ask_observed) = start_ask_server(ask_root.path().join("run"), &token);

    // `hire` first, because the other three need the agent it produces.
    let hired = cli()
        .arg("--json")
        .args(common::hire_args(fixture.state.path()))
        .run();
    assert_eq!(hired.code(), exit::SUCCESS, "hire\n{}", hired.output());
    let agent = hired
        .json::<dudes_proto::rpc::agent::HireResult>()
        .agent
        .to_string();

    let goal: Vec<String> = [
        "goal",
        "--agent",
        &agent,
        "--text",
        "make the tests pass",
        "--usd",
        "5",
        "--hop-cap",
        "8",
        "--",
        "/usr/bin/true",
    ]
    .iter()
    .map(|part| (*part).to_owned())
    .collect();

    let runs = [
        ("hire", hired),
        ("ping", cli().args(["--json", "ping"]).run()),
        ("goal", cli().arg("--json").args(&goal).run()),
        (
            "ask",
            Cli::new(
                ask_server.socket(),
                ask_root.path().join("no-credential").as_path(),
            )
            .env("DUDES_TOKEN", &token)
            .args(["--json", "ask", "--text", "which branch?"])
            .run(),
        ),
        (
            "why",
            cli().args(["--json", "why", "--agent", &agent]).run(),
        ),
        ("tail", cli().args(["--json", "tail", "--from", "0"]).run()),
    ];

    for (command, run) in &runs {
        assert_eq!(
            run.code(),
            exit::SUCCESS,
            "control: `{command}` actually ran, so its silence about the token means something\n{}",
            run.output()
        );
        assert!(
            !run.output().contains(&token),
            "`{command}` leaked the token\n{}",
            run.output()
        );
        assert!(
            !run.output().contains(&token[..12]),
            "`{command}` leaked a prefix of the token\n{}",
            run.output()
        );
    }

    // And every one of them produced output, so the absences above are absences *in* something.
    for (command, run) in &runs {
        assert!(
            !run.stdout.trim().is_empty(),
            "control: `{command}` printed a result\n{}",
            run.output()
        );
    }

    running.stop();
}
