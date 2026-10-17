//! Every effect, reached by driving the machine rather than by asserting a
//! `match` is exhaustive.
//!
//! A variant that compiles but that nothing can produce is dead surface, so every variant below
//! has an emitting path. The agent token's tests are in `tests/agent_token.rs`.

use std::collections::BTreeMap;

use dudes_proto::goal::Evidence;
use dudes_proto::{
    Actor, Entry, Event, EventKind, ExitStatus, PeerProvenance, Record, TurnOutcome,
};
use dudes_providers::launch::EnvVar;
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{ClientId, Command, Config, HookId, Input};
use dudes_types::{AgentId, Provenance, SessionId, Timestamp, TurnId};

fn prime_attached_session(harness: &mut Harness, agent: AgentId) {
    harness.append_replayed(Entry {
        at: Timestamp::from_unix_millis(1),
        record: Record::SessionLearned {
            agent,
            turn: TurnId::NIL,
            session: SessionId::from("550e8400-e29b-41d4-a716-446655440000"),
        },
    });
}

fn attach(agent: AgentId) -> Command {
    Command::Attach {
        agent,
        by: Actor::System,
        provenance: Some(PeerProvenance::NoRecordedAgent),
        cols: 80,
        rows: 24,
    }
}

/// `hook_port` is readable only for legacy entries. The machine's sole `Attached`
/// producer leaves it absent, and the contract serializer omits that `None` rather than writing a
/// new null field.
#[test]
fn a_new_attached_record_omits_the_legacy_hook_port() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    prime_attached_session(&mut harness, agent);
    let before = harness.trace.len();
    harness.drive(Input::Command(attach(agent)));
    let record = harness.trace[before..]
        .iter()
        .filter_map(Seen::record)
        .find(|record| matches!(record, dudes_proto::Record::Attached { .. }))
        .expect("the attach produced its durable record");
    let dudes_proto::Record::Attached { hook_port, .. } = record else {
        unreachable!("selected only Attached")
    };
    assert_eq!(*hook_port, None);
    assert!(
        serde_json::to_value(record)
            .unwrap()
            .get("hook_port")
            .is_none(),
        "new Attached records omit the legacy field"
    );
}

/// Checked input handling exposes `Persist`, `Spawn`, `Kill`, `Probe`, `RunPredicate`, `Reply`,
/// `Notify` and `OpenPty` through effects accessors. The old `ClosePty` effect remains a public
/// variant but has no producer now that confirmed-close superseded startup guessing.
///
/// One run of the machine reaches all eight active effects. Nothing is constructed by hand: each name in the
/// list below is there because some input made the machine ask for it.
///
/// `Probe` is last and is the only one that needed a second agent, because the machine will not
/// probe a live child of its own run — the runtime is that child's parent and will report its
/// exit. Reaching the emission therefore means reaching a child of a *previous* run, which is a
/// restart over a journal holding `TurnSpawned` without `TurnEnded`. What the answer would then
/// decide is `reaper.rs`'s subject and is switched off here, so this test can only fail for the
/// one reason it is named after.
#[test]
fn every_active_effect_variant_has_an_emitting_path() {
    let mut harness = Harness::new();

    // Notify needs a subscriber to notify.
    harness.drive(Input::Command(Command::Subscribe {
        client: ClientId(1),
    }));

    // Persist and Spawn: hiring and a goal decide `Continue`, which issues and launches a turn.
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    // Reply: a hook call is answered.
    harness.drive(Input::Hook {
        hook: HookId(1),
        event: Event {
            agent,
            at: Timestamp::from_unix_millis(0),
            provenance: Provenance::Observed,
            kind: EventKind::PreTool {
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            raw: None,
        },
    });

    // Notify: an agent asks a human, and every subscriber is told.
    //
    // Driven by `Command::Ask` — the one producer of `Effect::Notify` since the hook-branch was removed
    // the hook branch. The distribution is unchanged and only the producer moved: one emitter in
    // `machine.rs`, one `Seen::from` arm in `testing.rs`, one doc link in `input.rs`.
    harness.drive(Input::Command(Command::Ask {
        agent,
        text: "which branch?".into(),
    }));

    // Kill: a standing cause reaches the running child.
    harness.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));

    // RunPredicate: the turn ends, and the goal is judged.
    harness.drive(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: None,
            signal: Some(15),
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Failed {
            reason: "killed".into(),
        },
        last_message: None,
    });
    harness.drive(Input::PredicateDone {
        goal,
        pass: false,
        evidence: Evidence::Command {
            argv: vec!["npm".into(), "test".into()],
            program: Some("/opt/toolchain/bin/npm".into()),
            exit: None,
            output_tail: String::new(),
        },
    });

    // OpenPty: the keyboard is taken. Leave it attached through restart; the new run must
    // neither emit the old ClosePty effect nor guess that its PTY is absent.
    prime_attached_session(&mut harness, agent);
    harness.drive(Input::Command(attach(agent)));

    // Probe: a second agent's turn is left running, the daemon restarts, and crash rule (3) asks
    // the process table about the child it inherited. The answer is switched off, so this step is
    // about the emission and nothing else.
    let (second, _) = harness.hire_with_goal(&["npm", "test"]);
    assert!(
        harness.open_turn(second).is_some(),
        "a control: the second agent's child is running when the daemon dies"
    );
    harness.probe = false;
    harness.restart();

    let seen: Vec<&'static str> = harness.trace.iter().map(Seen::name).collect();
    for expected in [
        "Persist",
        "Spawn",
        "Kill",
        "Probe",
        "RunPredicate",
        "Reply",
        "Notify",
        "OpenPty",
    ] {
        assert!(
            seen.contains(&expected),
            "no path emitted {expected}; saw {seen:?}"
        );
    }
    assert!(
        !seen.contains(&"ClosePty"),
        "replay of an unconfirmed attachment must not ask a fresh host to close an unknown PTY"
    );
}

/// The daemon's own environment does not reach a formatter.
///
/// `Config` is one of the three structs converted from a raw `Vec<(OsString, OsString)>`,
/// and the worst of them: it holds the environment **before** any scrub, for the whole daemon run,
/// so an operator's `ANTHROPIC_API_KEY` was printable through a `{config:?}` from boot — before
/// `DUDES_TOKEN` exists at all. The other two, `LaunchSpec` and `AgentCtx`, are asserted in
/// `dudes-providers/tests/redaction.rs`, as is `UsageSource::OtelEnv` — a fourth holder that
/// conversion missed, converted. The CLI's `Environment` is a fifth holder, asserted
/// beside its resolver in `bins/dudes/src/env.rs`.
///
/// Three positive controls hold the absence claim up: the value really is in the struct, and the
/// formatted string really is this `Config` — it carries the data directory and the variable's
/// own name.
#[test]
fn a_formatted_config_prints_no_pre_scrub_daemon_env_value() {
    const SECRET: &str = "sk-ant-redaction-canary-value-do-not-print";

    let config = Config {
        daemon_env: vec![
            EnvVar::new("PATH", "/usr/bin"),
            EnvVar::new("ANTHROPIC_API_KEY", SECRET),
        ],
        ..Harness::config()
    };
    assert!(
        config
            .daemon_env
            .iter()
            .any(|var| var.expose_value() == SECRET),
        "a control: the config carries the value"
    );

    let printed = format!("{config:?}");
    assert!(
        printed.contains("/var/lib/dudes"),
        "the data directory must stay visible: {printed}"
    );
    assert!(
        printed.contains("ANTHROPIC_API_KEY"),
        "the variable's name must stay visible: {printed}"
    );
    assert!(
        !printed.contains(SECRET),
        "the daemon's pre-scrub environment reached a formatter: {printed}"
    );
}
