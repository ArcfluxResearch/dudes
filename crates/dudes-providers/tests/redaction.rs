//! No formatter prints an environment value.
//!
//! Four types under `crates/*/src` carry an environment: [`LaunchSpec`] (the child's, which is
//! where the agent-token plumbing puts `DUDES_TOKEN`), [`AgentCtx`], [`UsageSource::OtelEnv`] — a raw
//! `Vec<(String, String)>` until it became `Vec<EnvVar>` — and the supervisor's `Config`. `AgentCtx` and `Config`
//! hold the daemon's own, **before** the scrub, so an operator's `ANTHROPIC_API_KEY` is in them
//! from boot and was exposed before any Dudes token existed. The three that live in this crate are
//! tested here; `Config` is asserted next to the machine that holds it, in
//! `dudes-supervisor/tests/effects.rs`.
//!
//! `crates/` is the whole of that count, while two more holders live under `bins/`:
//! `duded::Settings` and `dudes::Environment`. `cargo xtask env-pairs` covers all of those holders;
//! this formatter test covers the three provider types, and the other formatter assertions live
//! beside the supervisor and CLI types they exercise.
//!
//! Every test here pairs the absence claim with a **positive control** from the same formatted
//! string, because an assertion that a needle is missing passes just as happily when the
//! formatter produced nothing at all.
//!
//! This file is in `tests/`, not `src/`, so a source scan over `src/` neither reads it nor is
//! confused by the secret-looking literals in it.

use std::path::Path;

use dudes_providers::adapter::{Adapter, AgentCtx, TurnSession, UsageSource};
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_types::{AgentId, ModelSelection, SessionId};

/// Not a credential: a literal chosen to be findable in a haystack, so a test that says "absent"
/// is saying something about redaction and not about a value that never arrived.
const SECRET: &str = "sk-ant-redaction-canary-value-do-not-print";
const AGENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

fn daemon_env() -> Vec<EnvVar> {
    vec![
        EnvVar::new("PATH", "/usr/bin"),
        EnvVar::new("ANTHROPIC_API_KEY", SECRET),
        EnvVar::new("DUDES_TOKEN", SECRET),
    ]
}

/// Formatting a `LaunchSpec` that carries a secret-looking environment value with
/// `{:?}` does not print the value.
///
/// Three controls hold the assertion up: the value is in the struct (read back through
/// `expose_value`, the accessor named so a call site handing a secret on is visible in review),
/// an argv element *is* in the output, and the variable's **name** is in the output too — so the
/// output is a real rendering of this spec, not an empty string or a rendering of some other one.
#[test]
fn a_formatted_launch_spec_prints_argv_and_env_names_but_no_env_value() {
    let spec = LaunchSpec {
        argv: vec!["claude".into(), "--".into(), "the task".into()],
        env: daemon_env(),
        cwd: Path::new("/tmp/dudes-fixture").to_path_buf(),
        stdin: dudes_providers::launch::LaunchInput::WriteThenClose(b"the task on stdin".to_vec()),
    };

    // Control 0: the secret really is in the struct, twice.
    let carried: Vec<&std::ffi::OsStr> = spec
        .env
        .iter()
        .filter(|var| var.expose_value() == SECRET)
        .map(EnvVar::name)
        .collect();
    assert_eq!(carried.len(), 2, "the spec carries the secret: {carried:?}");

    let printed = format!("{spec:?}");

    // Controls 1 and 2: the formatter produced this spec's argv and this spec's variable names.
    assert!(
        printed.contains("the task"),
        "argv must stay visible — a redacted spec would be undebuggable: {printed}"
    );
    assert!(
        printed.contains("ANTHROPIC_API_KEY") && printed.contains("DUDES_TOKEN"),
        "environment names must stay visible: {printed}"
    );

    // The claim.
    assert!(
        !printed.contains(SECRET),
        "an environment value reached a formatter: {printed}"
    );
    // And the bytes on stdin are not printed either — only how many there are.
    assert!(
        !printed.contains("the task on stdin"),
        "stdin bytes reached a formatter: {printed}"
    );
    assert!(
        printed.contains("stdin: WriteThenClose(17)"),
        "stdin is described by its length: {printed}"
    );
}

/// The pre-scrub daemon environment does not reach a
/// formatter either. `AgentCtx` is the higher-severity half of the leak — it holds the operator's
/// environment *before* `scrub_env` runs, so `ANTHROPIC_API_KEY` was printable through it today,
/// before `DUDES_TOKEN` is ever minted.
///
/// Positive controls from the same string: the agent id and the working directory, both
/// non-secret fields of this same struct.
#[test]
fn a_formatted_agent_ctx_prints_no_pre_scrub_daemon_env_value() {
    let env = daemon_env();
    let ctx = AgentCtx {
        extra_dirs: &[],
        agent: AgentId::parse(AGENT).unwrap(),
        cwd: Path::new("/tmp/dudes-fixture"),
        agent_dir: Path::new("/tmp/dudes-data/agents/01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        daemon_env: &env,
        sandbox: None,
        trusted: false,
        max_turns: None,
        max_budget: None,
        mcp_servers: &[],
    };

    let printed = format!("{ctx:?}");

    assert!(
        printed.contains(AGENT) && printed.contains("/tmp/dudes-fixture"),
        "the agent and its cwd must stay visible: {printed}"
    );
    assert!(
        printed.contains("ANTHROPIC_API_KEY"),
        "environment names must stay visible: {printed}"
    );
    assert!(
        !printed.contains(SECRET),
        "the daemon's pre-scrub environment reached a formatter: {printed}"
    );
}

/// The redaction is a property of the type, not of the two structs above: a `Vec<EnvVar>` printed
/// on its own, a single `EnvVar`, and the environment of a spec an adapter actually built all
/// keep the value in. This is what "survives someone adding a second env-carrying struct"
/// means, stated as a check rather than as prose.
#[test]
fn the_value_is_redacted_wherever_an_env_var_is_printed() {
    let one = EnvVar::new("DUDES_TOKEN", SECRET);
    let printed = format!("{one:?}");
    assert_eq!(printed, r#""DUDES_TOKEN"=<redacted>"#);

    let many = daemon_env();
    let printed = format!("{many:?}");
    assert!(printed.contains("PATH"), "a control: {printed}");
    assert!(!printed.contains(SECRET), "a bare Vec leaked: {printed}");

    let env = daemon_env();
    let ctx = AgentCtx {
        extra_dirs: &[],
        agent: AgentId::parse(AGENT).unwrap(),
        cwd: Path::new("/tmp/dudes-fixture"),
        agent_dir: Path::new("/tmp/dudes-data/agents/01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        daemon_env: &env,
        sandbox: None,
        trusted: false,
        max_turns: None,
        max_budget: None,
        mcp_servers: &[],
    };
    let spec = ClaudeAdapter::default()
        .managed(
            &ctx,
            "the task",
            &TurnSession::First(SessionId::from(UUID)),
            &ModelSelection::provider_default(),
        )
        .expect("a non-empty task without NUL is a launch");
    assert!(
        spec.env.iter().any(|var| var.expose_value() == SECRET),
        "a control: the adapter passed the value through to the child"
    );
    let printed = format!("{spec:?}");
    assert!(printed.contains(UUID), "a control: {printed}");
    assert!(
        !printed.contains(SECRET),
        "a built launch leaked: {printed}"
    );
}

/// [`UsageSource::OtelEnv`] prints its variable names and
/// no value, in `{:?}` and in `{:#?}`.
///
/// The variant renders correctly today and nothing said so, which is a different thing from being
/// safe. Replacing the derive on `UsageSource` (`src/adapter.rs:83`) with a hand-written `Debug`
/// that prints `expose_value()` — the ordinary readability edit — left all 72 `dudes-providers`
/// tests green while printing `OTEL_EXPORTER_OTLP_HEADERS=Bearer sk-live-DEADBEEF`. Neither source
/// guard reaches that edit: the local purity rule wants a `Debug` format spec on the line, and
/// `cargo xtask env-pairs` wants a raw representation, which this field stopped being when it
/// moved to `Vec<EnvVar>`.
///
/// That nothing constructs the variant is not a reason to leave it out. The `LaunchSpec` test
/// above hand-builds its subject around a fixture secret no production code produces either; what
/// both assert is a property of a formatter, and a formatter does not need a caller to be wrong.
///
/// Both format specs, because they are two paths through a derived `Debug` and a hand-written one
/// can get `{:?}` right and `{:#?}` wrong.
#[test]
fn a_formatted_usage_source_prints_env_names_but_no_env_value() {
    let source = UsageSource::OtelEnv(vec![
        EnvVar::new(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "http://collector.invalid:4318",
        ),
        EnvVar::new("OTEL_EXPORTER_OTLP_HEADERS", SECRET),
    ]);

    // Control 0: the secret really is in the variant, so "absent" below is about the formatter.
    let UsageSource::OtelEnv(carried) = &source else {
        panic!("the fixture is an OtelEnv");
    };
    assert!(
        carried.iter().any(|var| var.expose_value() == SECRET),
        "the fixture carries the secret"
    );

    for printed in [format!("{source:?}"), format!("{source:#?}")] {
        // Controls 1 and 2: this variant, and this variant's names, really were rendered — an
        // empty string would pass the claim below on its own.
        assert!(
            printed.contains("OtelEnv"),
            "the variant must stay visible: {printed}"
        );
        assert!(
            printed.contains("OTEL_EXPORTER_OTLP_HEADERS")
                && printed.contains("OTEL_EXPORTER_OTLP_ENDPOINT"),
            "environment names must stay visible: {printed}"
        );

        // The claim.
        assert!(
            !printed.contains(SECRET),
            "an environment value reached a formatter: {printed}"
        );
    }
}
