//! The operator credential's **read** and its **presentation** are two separately-named
//! functions, with an `OperatorCredential` value passing between them. They are not one function
//! that opens a path and returns a `Response`.
//!
//! # This test is preservation, not work
//!
//! The path-token check finds **0 against a positive control**, so it costs nothing today and goes
//! red only if a future diff grows a path on the wire type or folds the credential read into the
//! call. What is below is a re-run of that check, not a second copy of the check.
//!
//! # What is actually asserted here, and what the compiler asserts for free
//!
//! Two things this file can check that a grep cannot:
//!
//! 1. **The two functions exist under those names and have those types.** Taken as function
//!    pointers, so a signature change is a compile error rather than a stale doc comment. A
//!    collapsed door — one function opening a path and returning a `Response` — could not satisfy
//!    both bindings.
//! 2. **The value really passes between them.** The credential read by the first is presented by the
//!    second, over a real socket, and the daemon admits the call. A pair of functions that were
//!    *named* right but not *wired* together would pass assertion 1 and fail this one.
//!
//! And one thing it re-measures: the path-token count over the wire type, with a positive
//! control beside it.
//!
//! # The envelope gets its own arm
//!
//! The door may be local; the doorway must not be. The wire contract encodes no locality, while
//! the behaviour reads a local socket path and a local file. The arm below asserts the envelope this
//! binary actually sends carries no path, uid, pid or hostname — from the serialised request, not
//! from a reading of the struct.
#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;

use dudes::exit;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::{JSONRPC, Method, Request, RequestId, Response};
use dudes_rpc::{Client, ClientError};

mod common;

use common::{Cli, Fixture, OpenRunner, SharedHost, start};

/// **The read half exists, under that name, with that type.**
///
/// A path in, an `Option<OperatorCredential>` out. It returns no `Response` and takes no method —
/// which the type states and a comment could only claim.
#[test]
fn the_read_half_takes_a_path_and_returns_a_credential() {
    let read: fn(&Path) -> Result<Option<OperatorCredential>, dudes::CredentialError> =
        dudes::read_operator_credential;

    let dir = tempfile::tempdir().expect("a directory");
    assert!(
        read(dir.path())
            .expect("an absent credential is not an error")
            .is_none(),
        "no file, no credential"
    );

    std::fs::write(
        dir.path().join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        "c".repeat(dudes_proto::rpc::cred::OPERATOR_CREDENTIAL_LEN),
    )
    .expect("write a credential");
    assert!(
        read(dir.path()).expect("a well-formed one reads").is_some(),
        "positive control: the read half really reads"
    );
}

/// **The presentation half exists, under that name, with that type.**
///
/// A credential *value* in, a `Response` out. It takes no path and no directory — so it cannot be
/// the same function as the one above, and cannot have quietly absorbed it.
#[test]
fn the_presentation_half_takes_a_credential_value_and_no_path() {
    type Half = fn(
        &mut Client<
            std::io::BufReader<std::os::unix::net::UnixStream>,
            std::os::unix::net::UnixStream,
        >,
        Method,
        Option<serde_json::Value>,
        Option<OperatorCredential>,
    ) -> Result<Response, ClientError>;

    // The coercion is the assertion: this compiles only while the signature is what the
    // credential-read/presentation split requires. Asserted against a null pointer comparison rather than bound to `_`, so the value is
    // used and clippy's `no_effect_underscore_binding` has nothing to complain about — a function
    // item coerced to a pointer is never null, which is what makes this a real (if trivially true)
    // use of the value the type check produced.
    let present: Half = dudes::call_presenting;
    assert!(
        !std::ptr::eq(present as *const (), std::ptr::null()),
        "the presentation half is a function of exactly that type"
    );
}

/// **The value passes between them, over a real socket.**
///
/// Read here, presented there, admitted by the daemon. The refusal arm is the control: the same
/// call, the same daemon, with nothing read, is refused — so the admission is *because of* the value
/// that crossed and not because the method was open.
#[test]
fn the_credential_read_by_one_half_is_what_the_other_half_presents() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let mut running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );

    // --- the read half --------------------------------------------------------------------------
    let credential = dudes::read_operator_credential(&settings.runtime_dir)
        .expect("the daemon minted a credential")
        .expect("and it is there");

    // --- the presentation half, with the value from above ----------------------------------------
    let mut client = Client::connect(settings.socket()).expect("connect");
    let answered = dudes::call_presenting(
        &mut client,
        Method::AgentHire,
        Some(
            serde_json::to_value(hire_params(fixture.state.path())).expect("the params serialise"),
        ),
        Some(credential),
    )
    .expect("the call is answered");
    assert!(
        answered.error.is_none(),
        "the credential that crossed is the one the daemon recognises: {:?}",
        answered.error
    );

    // --- the control: the same call, presenting nothing -------------------------------------------
    let mut second = Client::connect(settings.socket()).expect("connect");
    let refused = dudes::call_presenting(
        &mut second,
        Method::AgentHire,
        Some(
            serde_json::to_value(hire_params(fixture.state.path())).expect("the params serialise"),
        ),
        None,
    )
    .expect("the call is answered, with an error");
    assert!(
        refused.error.is_some(),
        "control: without the value, the same call is refused"
    );

    running.stop();
}

/// The door may be local; the doorway must not be.
///
/// The envelope this binary sends is serialised and inspected: it carries a method, params, an id
/// and an optional credential, and **no path, uid, pid or hostname**. Asserted over the JSON rather
/// than over the struct's fields, because the wire is what an outside implementation sees.
///
/// The *behaviour* is unashamedly local and the arm above proves it: a socket path out of an
/// environment variable, a credential out of a file. Neither reaches the wire.
#[test]
fn the_envelope_carries_no_locality() {
    let credential =
        OperatorCredential::new("c".repeat(dudes_proto::rpc::cred::OPERATOR_CREDENTIAL_LEN))
            .expect("well formed");
    let request = Request {
        jsonrpc: JSONRPC.to_owned(),
        id: RequestId::Number(1),
        method: Method::AgentHire.name().to_owned(),
        params: None,
        v: None,
        cred: Some(credential),
    };
    let value = serde_json::to_value(&request).expect("the envelope serialises");
    let keys: Vec<&str> = value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();

    assert!(
        keys.contains(&"cred"),
        "positive control: the credential *is* on the envelope — {keys:?}"
    );
    for local in [
        "path", "socket", "uid", "gid", "pid", "host", "hostname", "cwd",
    ] {
        assert!(
            !keys.contains(&local),
            "the envelope grew a locality field `{local}`: {keys:?}"
        );
    }
    assert_eq!(
        keys.len(),
        4,
        "id, jsonrpc, method, cred — and nothing else: {keys:?}"
    );
}

/// Whether `line` contains `needle` bounded by non-word characters — the path-token grep's `\b`.
///
/// The path-token grep is `PathBuf|Path\b|File\b|fs::|read_to_string|OpenOptions|env::`. Approximating
/// `Path\b` as `Path>` and `Path)` would be strictly narrower and would miss a plain
/// `use std::path::Path;`.
fn contains_word(line: &str, needle: &str) -> bool {
    line.match_indices(needle).any(|(at, _)| {
        let before = line[..at].chars().next_back();
        let after = line[at + needle.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
        boundary(before) && boundary(after)
    })
}

/// Re-measured here rather than restated: the wire type names no path.
///
/// **The check itself lives with the wire type.** What this test adds is that it runs in the suite that would break
/// it, with a positive control beside it — a zero from a grep whose corpus had moved
/// would otherwise read exactly like a zero from a clean tree.
#[test]
fn the_wire_type_names_no_path_and_the_control_does() {
    /// The own needles: the tokens that would mean the wire type had learned about a
    /// filesystem. Kept as one list so the measurement and its control cannot be run against
    /// different questions.
    ///
    /// # `Path` and `File` are matched on a word boundary, because the grep's are
    ///
    /// The path-token grep is `PathBuf|Path\b|File\b|fs::|read_to_string|OpenOptions|env::`.
    /// Needles of `Path>` and `Path)` would be strictly narrower: adding `use std::path::Path;` to
    /// `crates/dudes-proto/src/rpc/cred.rs` would take the grep from 0 to 1 and leave the test
    /// green.
    ///
    /// [`WORD_NEEDLES`] are matched with [`contains_word`] so `Path;`, `Path,`, `Path>` and a bare
    /// `Path` all count, and `PathBuf` is caught by its own literal rather than by this rule.
    const NEEDLES: &[&str] = &["PathBuf", "fs::", "read_to_string", "OpenOptions", "env::"];

    /// Needles the grep spells with `\b`, matched on a word boundary here.
    const WORD_NEEDLES: &[&str] = &["Path", "File"];

    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root")
        .to_path_buf();

    let count = |relative: &str| -> usize {
        let path = repo.join(relative);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
        text.lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .filter(|line| {
                NEEDLES.iter().any(|needle| line.contains(needle))
                    || WORD_NEEDLES
                        .iter()
                        .any(|needle| contains_word(line, needle))
            })
            .count()
    };

    // The needle set is sharp enough to see a bare import, which a narrower set
    // would miss. Asserted against a synthetic line rather than a file, so it holds whatever the two
    // files below happen to contain.
    assert!(
        contains_word("use std::path::Path;", "Path"),
        "a bare `use std::path::Path;` must count — it is what the grep's `Path\\b` catches"
    );
    assert!(
        !contains_word("let p = PathBuf::new();", "Path"),
        "and `PathBuf` is caught by its own literal, not by the word rule"
    );

    let control = count("crates/dudes-rpc/src/server.rs");
    assert!(
        control > 0,
        "control: the transport does name paths, so a zero below means something"
    );

    let measured = count("crates/dudes-proto/src/rpc/cred.rs");
    assert_eq!(
        measured, 0,
        "the wire type learned where it came from (control {control})"
    );
}

/// `agent.hire` params for the tests above.
fn hire_params(cwd: &Path) -> dudes_proto::rpc::agent::HireParams {
    dudes_proto::rpc::agent::HireParams {
        project: None,
        character: None,
        accent: None,
        name: "Hector".to_owned(),
        role: "infra".to_owned(),
        department: "infra".to_owned(),
        provider: dudes_types::Provider::Claude,
        cwd: cwd.to_path_buf(),
        budget: dudes_proto::budget::AgentBudget {
            usd_per_day: Some(dudes_types::Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        model: None,
        trusted: false,
        checkout: None,
        from: None,
        mcp_servers: Vec::new(),
    }
}

/// The CLI's copy of the socket name equals the daemon's, and the CLI finds the daemon without being
/// told the file name.
///
/// `bins/dudes/src/env.rs` spells `duded.sock` because a client cannot import a constant from a
/// package it only dev-depends on. This is the one place that drift is visible — and the second arm
/// is what makes it more than a string comparison: the CLI, given only the runtime directory, reaches
/// a daemon it was never told the socket path of.
#[test]
fn the_socket_name_matches_the_daemons_and_the_cli_finds_it() {
    assert_eq!(
        dudes::env::SOCKET_NAME,
        duded::settings::SOCKET_NAME,
        "the CLI is looking for a file the daemon does not create"
    );

    let fixture = Fixture::new();
    let settings = fixture.settings();
    let mut running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );

    // No `DUDES_SOCK` at all: the CLI derives the socket from the runtime directory.
    let found = Cli::new(&settings.socket(), &settings.runtime_dir)
        .without("DUDES_SOCK")
        .args(["ping"])
        .run();
    assert_eq!(
        found.code(),
        exit::SUCCESS,
        "the CLI found the socket from the runtime directory alone\n{}",
        found.output()
    );

    running.stop();
}
