//! the agent token, from the mint to the child's environment.
//!
//! One subject, one file. What these tests pin is a chain rather than a value: a token is drawn
//! once per agent per daemon run, its **hash** is journaled and its plaintext never is, the
//! record carrying that hash is acknowledged **before** any child of that agent is launched, and
//! the plaintext reaches the child through `LaunchSpec.env` — never argv, never a formatter.
//!
//! The first five are positive, not an inverted tripwire: an assertion that "a token *does* reach a
//! child" is satisfied by a token that is wrong, late, or written to the log.
//!
//! The sixth pins *which* token a child is armed with when more than one draw is outstanding, and
//! carries the [`InOrder`] driver that makes "the caller honoured the acknowledgement-ordering
//! contract" an assertion rather than a claim.
//!
//! The last three turn the chain around and pin the other end: not what a child is *armed* with,
//! but what a *presented* hash resolves to. That is a fold of the journal rather than the run's
//! plaintext, which is why it survives a restart and why a later draw evicts nothing.

use std::collections::BTreeMap;

use dudes_proto::goal::Evidence;
use dudes_proto::record::Hex32;
use dudes_proto::recovery::RecoveryAttribution;
use dudes_proto::rpc::hook::HOOK_TOKEN_LEN;
use dudes_proto::{Actor, Entry, ExitStatus, PeerProvenance, Record, RecordKind, TurnOutcome};
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_supervisor::input::{RecoveryChoice, RecoveryCommand};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{Command, Effect, Input, MintedFor, PersistToken, blake3_hex};
use dudes_types::{AgentId, GoalId, Lsn, SessionId, Timestamp, TurnId};

// ---- reading a transcript -------------------------------------------------------------------

/// The spec of every launching effect in `trace`, managed and attached alike.
///
/// One function over both variants, because the token is armed the same way for both: "`LaunchSpec.env`
/// gains two pairs for every child the daemon launches on an agent's behalf, managed and attached
/// alike". A helper that read only `Seen::Spawn` would let the attached half rot.
fn launched(trace: &[Seen]) -> Vec<&LaunchSpec> {
    trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::Spawn { spec, .. } | Seen::OpenPty { spec, .. } => Some(spec),
            _ => None,
        })
        .collect()
}

/// The index of every launching effect in `trace`.
fn launch_positions(trace: &[Seen]) -> Vec<usize> {
    trace
        .iter()
        .enumerate()
        .filter(|(_, seen)| matches!(seen, Seen::Spawn { .. } | Seen::OpenPty { .. }))
        .map(|(at, _)| at)
        .collect()
}

/// Every `token_hash` the machine asked to have written for `agent`, in order.
fn mints(trace: &[Seen], agent: AgentId) -> Vec<String> {
    trace
        .iter()
        .filter_map(Seen::record)
        .filter_map(|record| match record {
            Record::HookTokenMinted {
                agent: a,
                token_hash,
            } if *a == agent => Some(token_hash.clone()),
            _ => None,
        })
        .collect()
}

/// The index of the first `Persist{HookTokenMinted}` in `trace`.
fn first_mint_position(trace: &[Seen]) -> Option<usize> {
    trace
        .iter()
        .position(|seen| matches!(seen.record(), Some(Record::HookTokenMinted { .. })))
}

/// One environment value by name.
fn value_of<'a>(spec: &'a LaunchSpec, name: &str) -> Option<&'a std::ffi::OsStr> {
    spec.env
        .iter()
        .find(|var| var.name() == name)
        .map(EnvVar::expose_value)
}

/// The token text a child was actually handed. Read out of the composed pair rather than guessed,
/// so every assertion below searches for the string that exists rather than one a test invented.
fn token_of(spec: &LaunchSpec) -> Option<String> {
    value_of(spec, "DUDES_TOKEN")?.to_str().map(str::to_owned)
}

fn env_names(spec: &LaunchSpec) -> Vec<String> {
    spec.env
        .iter()
        .map(|var| var.name().to_string_lossy().into_owned())
        .collect()
}

// ---- driving --------------------------------------------------------------------------------

fn confirmed_session(agent: AgentId) -> Input {
    Input::Replayed {
        lsn: Lsn(1),
        entry: Entry {
            at: Timestamp::from_unix_millis(1),
            record: Record::SessionLearned {
                agent,
                turn: TurnId::NIL,
                session: SessionId::from("550e8400-e29b-41d4-a716-446655440000"),
            },
        },
    }
}

fn attach_command(agent: AgentId) -> Command {
    Command::Attach {
        agent,
        by: Actor::System,
        provenance: Some(PeerProvenance::NoRecordedAgent),
        cols: 80,
        rows: 24,
    }
}

fn attach(harness: &mut Harness, agent: AgentId) {
    harness.drive(confirmed_session(agent));
    harness.drive(Input::Command(attach_command(agent)));
}

/// End a turn failing and judge its goal failing, which is what makes the loop issue the next one.
fn fail_turn(harness: &mut Harness, turn: TurnId, goal: GoalId) {
    harness.drive(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: Some(1),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Failed {
            reason: "the tests failed".into(),
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
}

fn complete_turn(harness: &mut Harness, turn: TurnId, goal: GoalId) {
    harness.drive(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: None,
    });
    harness.drive(Input::PredicateDone {
        goal,
        pass: true,
        evidence: Evidence::Command {
            argv: vec!["npm".into(), "test".into()],
            program: Some("/opt/toolchain/bin/npm".into()),
            exit: Some(0),
            output_tail: String::new(),
        },
    });
}

// ---- the five that replace the tripwire -------------------------------------------------------

/// A managed child's `LaunchSpec.env` carries both `DUDES_SOCK` and `DUDES_TOKEN`.
///
/// **The pair is asserted as a pair, in one assertion.** Two assertions would each pass while the
/// other half was silently absent, and the two halves are useless apart: a socket path with no
/// token reaches a daemon that will not name the caller, and a token with no socket path names a
/// caller that cannot find the daemon.
///
/// The attached arm is not a bonus. The token is armed "managed and attached alike", and `agent.attach` is a
/// second launching site — a rule implemented at only one of them is half implemented.
///
/// The last assertion is "one token per agent": the attach **reuses** what the managed turn
/// minted. Two tokens would still satisfy every clause above and still be wrong.
#[test]
fn a_managed_child_carries_the_socket_and_the_token() {
    let mut config = Harness::config();
    config
        .daemon_env
        .push(EnvVar::new("DUDES_MAILBOX", "/inherited/mailbox"));
    let mut harness = Harness::with_config(
        config,
        dudes_supervisor::testing::FakeClock::ticking(1_787_394_600_000, 1),
        dudes_supervisor::testing::FakeHost::wall_clock_anchored(),
        || Box::new(dudes_providers::claude::ClaudeAdapter::default()),
    );
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("managed turn");
    complete_turn(&mut harness, turn, goal);
    attach(&mut harness, agent);

    let managed: Vec<&LaunchSpec> = harness
        .trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::Spawn { spec, .. } => Some(spec),
            _ => None,
        })
        .collect();
    let attached: Vec<&LaunchSpec> = harness
        .trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::OpenPty { spec, .. } => Some(spec),
            _ => None,
        })
        .collect();
    assert_eq!(
        managed.len(),
        1,
        "a control: one managed child was launched"
    );
    assert_eq!(
        attached.len(),
        1,
        "a control: one attached child was launched"
    );

    for (which, spec) in [("managed", managed[0]), ("attached", attached[0])] {
        let names = env_names(spec);
        // `count() == 1`, not `any()`: `any()` is satisfied by a spec carrying the pair *twice*,
        // which is the shape an env composed by appending produces when something upstream already
        // supplied one — and `getenv` answers the first match, not the one composed here.
        let count = |wanted: &str| names.iter().filter(|name| name.as_str() == wanted).count();
        assert!(
            count("DUDES_SOCK") == 1 && count("DUDES_TOKEN") == 1 && count("DUDES_MAILBOX") == 1,
            "{which}: DUDES_SOCK and DUDES_TOKEN travel together, exactly once each, or not at \
             all; saw {names:?}"
        );
        assert_eq!(
            value_of(spec, "DUDES_SOCK"),
            Some(Harness::config().socket.as_os_str()),
            "{which}: the socket the child is told to call is the one the daemon listens on"
        );
        assert_eq!(
            value_of(spec, "DUDES_MAILBOX"),
            Some(
                Harness::config()
                    .data_dir
                    .join("agents")
                    .join(agent.to_string())
                    .as_os_str()
            )
        );
        assert!(
            !spec
                .argv
                .iter()
                .any(|part| part.to_string_lossy().contains("DUDES_MAILBOX"))
        );
        assert_eq!(
            token_of(spec).expect("the token is text").len(),
            HOOK_TOKEN_LEN,
            "{which}: the value is a whole token"
        );
        assert!(
            !spec
                .argv
                .iter()
                .any(|part| part.to_string_lossy().contains("DUDES_TOKEN")),
            "{which}: argv is world-readable and the token never goes in it"
        );
    }

    assert_eq!(
        token_of(managed[0]),
        token_of(attached[0]),
        "one agent, one token for the run: `agent.attach` reuses and mints nothing"
    );
}

/// `Persist{HookTokenMinted}` precedes the first `Effect::Spawn` or
/// `Effect::OpenPty` of the run for that agent, where "precedes" means *acknowledged
/// before*, not *emitted earlier in the same batch*.
///
/// That is the crate's general rule — `effect.rs`: "the machine emits a `Effect::Persist` first
/// and the acting effect only on its acknowledgement", one exception, `Probe` — and it applies to
/// this record: both must be fsynced before the `Spawn` they arm.
///
/// **So the assertion is an absence held open by `Harness::unfsynced`, not a comparison of
/// indices.** With the mint's acknowledgement withheld, no launching effect may appear *at all*.
/// An index comparison could only see two effects in one returned slice and would stay green
/// against a machine that emitted both together — which is exactly the shape that is forbidden,
/// because a crash between the two would leave a child holding a credential that is not in the
/// journal.
///
/// Two controls stop the absence from being vacuous: the machine is shown to have *reached* the
/// mint in the held run, and the same journal is then shown to reach the launch once the record
/// is allowed onto the disk.
#[test]
fn the_mint_precedes_the_first_spawn_of_the_run() {
    // ---- managed ----
    let mut harness = Harness::new();
    harness.unfsynced.insert(RecordKind::HookTokenMinted);
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);

    assert_eq!(
        mints(&harness.trace, agent).len(),
        1,
        "a control: the machine reached the mint and asked for it to be written"
    );
    assert!(
        launch_positions(&harness.trace).is_empty(),
        "a child was launched while the record that arms it was not on the disk; saw {:?}",
        harness.trace.iter().map(Seen::name).collect::<Vec<_>>()
    );

    // The control: the same journal, with the record allowed through, reaches the launch — and
    // the mint is in front of it.
    harness.unfsynced.clear();
    let boundary = harness.trace.len();
    let recovery_from = harness.trace.len();
    harness.restart();
    assert!(
        harness.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "restart alone does not repeat interrupted goal work"
    );
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert!(
        harness.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "plain Resume does not supply recovery consent"
    );
    let expected = harness
        .supervisor
        .recovery_opportunity(agent)
        .unwrap()
        .expected;
    let handled = harness.recover(RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    });
    assert_eq!(handled.recovery().expect("command progress").refusal, None);
    assert!(harness.journal.iter().any(|entry| matches!(&entry.record,
        Record::RecoveryAuthorized(authorization) if authorization.request_id == Hex32::new([41; 32]))));

    let run_two = &harness.trace[boundary..];
    let mint = first_mint_position(run_two).expect("the second run mints");
    let launch = *launch_positions(run_two)
        .first()
        .expect("a control: the launch does go out once the mint is acknowledged");
    assert!(
        mint < launch,
        "the mint must precede the launch it arms: {mint} vs {launch}"
    );

    // ---- attached ----
    let mut held = Harness::new();
    held.unfsynced.insert(RecordKind::HookTokenMinted);
    let agent = held.hire();
    attach(&mut held, agent);
    assert_eq!(
        mints(&held.trace, agent).len(),
        1,
        "a control: the attach reached the mint"
    );
    assert!(
        launch_positions(&held.trace).is_empty(),
        "a PTY was opened while the record that arms it was not on the disk; saw {:?}",
        held.trace.iter().map(Seen::name).collect::<Vec<_>>()
    );

    let mut control = Harness::new();
    let agent = control.hire();
    attach(&mut control, agent);
    let mint = first_mint_position(&control.trace).expect("the attach mints");
    let launch = *launch_positions(&control.trace)
        .first()
        .expect("a control: the PTY does open once the mint is acknowledged");
    assert!(
        mint < launch,
        "the mint must precede the PTY it arms: {mint} vs {launch}"
    );
}

/// No `{effect:?}` prints the token in clear.
///
/// This test pins `Effect`'s hand-written `Debug` and `EnvVar`'s redaction with a token
/// actually in a spec. The effect is rebuilt
/// from the recorded parts rather than constructed by hand, so the subject is the environment the
/// machine really composed.
///
/// **The control is the one that matters.** `printed.contains("DUDES_TOKEN")` — the *key* — is
/// what stops the absence assertion from passing against a formatter that dropped `env` entirely,
/// which is the cheap way to make a redaction test green while deleting the information a trace
/// line exists for. The plaintext's length is the second control: `contains("")` is true of every
/// string.
#[test]
fn no_effect_debug_prints_the_token_plaintext() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("managed turn");
    complete_turn(&mut harness, turn, goal);
    attach(&mut harness, agent);

    let spawn = harness
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::Spawn {
                turn,
                spec,
                install,
                mcp_grant,
            } => Some(Effect::Spawn {
                turn: *turn,
                spec: spec.clone(),
                install: install.clone(),
                mcp_grant: *mcp_grant,
            }),
            _ => None,
        })
        .expect("a managed child was launched");
    let open_pty = harness
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::OpenPty { agent, spec } => Some(Effect::OpenPty {
                agent: *agent,
                spec: spec.clone(),
                install: dudes_providers::adapter::HookInstall::default(),
                cols: 80,
                rows: 24,
            }),
            _ => None,
        })
        .expect("an attached child was launched");

    for effect in [spawn, open_pty] {
        let spec = match &effect {
            Effect::Spawn { spec, .. } | Effect::OpenPty { spec, .. } => spec,
            other => unreachable!("{}", other.name()),
        };
        let plaintext = token_of(spec).expect("the spec carries the token");
        assert_eq!(
            plaintext.len(),
            HOOK_TOKEN_LEN,
            "a control: the string searched for is a whole token, so this is not `contains(\"\")`"
        );

        let printed = format!("{effect:?}");
        assert!(
            printed.contains("DUDES_TOKEN"),
            "a control: the same formatting prints the env *key*, so the assertion below cannot \
             pass by the formatter having dropped the field: {printed}"
        );
        assert!(
            !printed.contains(&plaintext),
            "an Effect's Debug printed the token in clear: {printed}"
        );
    }
}

/// The run's `HookTokenMinted` carries the **hash**, and the plaintext appears
/// nowhere in the journal — **both arms asserted**.
///
/// An absence-only assertion is satisfied by a mint that never happened, which is why the hash arm
/// comes first and is tied to the plaintext by construction: the string searched for is read out
/// of the composed `DUDES_TOKEN` value, and the hash asserted is `blake3_hex` of that same string.
///
/// **The guarantee this pins is narrower than "the type system forbids it", and saying so is the
/// point.** `HookToken` *is* `Serialize` and serialises to its plaintext. What holds is that no
/// `Record` variant carries a `HookToken` — `HookTokenMinted.token_hash` is a `String`. Over
/// `crates/*/src` and `bins/*/src`, the four non-test `HookToken::expose()` sites are
/// `bins/duded/src/boot.rs`, `bins/duded/src/classify.rs`,
/// `crates/dudes-supervisor/src/machine.rs`, and `crates/dudes-supervisor/src/token.rs`. Neither is a
/// type error, so this test checks every record the machine asked to write at runtime.
#[test]
fn the_journal_carries_the_hash_and_never_the_plaintext() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("managed turn");
    complete_turn(&mut harness, turn, goal);
    attach(&mut harness, agent);

    let specs = launched(&harness.trace);
    assert_eq!(specs.len(), 2, "a control: two children were launched");
    let plaintext = token_of(specs[0]).expect("the spec carries the token");
    assert_eq!(
        plaintext.len(),
        HOOK_TOKEN_LEN,
        "a control: a whole token, so the search below is not for an empty string"
    );

    // The arm that makes the absence non-vacuous: a mint really happened, and what it journaled
    // is the digest of the very plaintext the child was handed.
    assert!(
        mints(&harness.trace, agent).contains(&blake3_hex(plaintext.as_bytes())),
        "no HookTokenMinted carries blake3 of the token the child was handed; saw {:?}",
        mints(&harness.trace, agent)
    );

    // And the absence itself, over everything the machine asked to write — a superset of what
    // reached the disk, so a record dropped by a crash is still covered.
    for record in harness.records() {
        let json = serde_json::to_string(record).expect("a record serialises");
        assert!(
            !json.contains(&plaintext),
            "the token plaintext reached the journal: {json}"
        );
    }
}

/// A second daemon run mints again for an agent that is still live: validity is set membership in
/// the hashes journaled for a non-retired agent, not recency.
///
/// The plaintext is not journaled, so it does not survive a restart and the next run has
/// nothing to hand a child but a fresh draw. A rule that reused "the agent's current token" would
/// refuse every call from a child adopted across that restart — after a crash only, for adopted
/// children only, looking exactly like a permissions bug.
///
/// **The control is why run 1 launches twice.** "Run 1 minted exactly once" is the assertion that
/// separates *mints again per run* from *mints on every launch* — and with a single launch in run
/// 1 it is satisfied by both. Two launches make it discriminating.
///
/// **What proves the second run *drew* is the mint count over the post-restart slice, not the
/// inequality of the hashes.** Replay emits no `Persist` at all, so a `HookTokenMinted` appearing
/// after the boundary can only be a fresh emission; the inequality is carried by `FakeRng`'s
/// counter landing elsewhere after `restart()` rather than by the property, and it is labelled as
/// corroboration at the assertion itself. The run-wide count of two is set membership: the first
/// hash is still there. Nothing prunes it. The set only grows, and validity stays set
/// membership rather than recency.
#[test]
fn a_second_daemon_run_mints_again_for_a_live_agent() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let first = harness.open_turn(agent).expect("turn 1 was issued");

    // Turn 1 fails, the goal is judged failing, and the loop issues turn 2.
    fail_turn(&mut harness, first, goal);
    let second = harness.open_turn(agent).expect("turn 2 was issued");
    assert_ne!(first, second, "a control: a second turn, not the same one");
    assert_eq!(
        launch_positions(&harness.trace).len(),
        2,
        "a control: run 1 launched twice, so 'exactly one mint' below discriminates"
    );
    let run_one = mints(&harness.trace, agent);
    assert_eq!(
        run_one.len(),
        1,
        "run 1 minted once for two launches: a token is per agent per run, not per launch"
    );

    harness.launch = false;
    fail_turn(&mut harness, second, goal);
    let third = harness.open_turn(agent).expect("turn 3 was issued");
    assert_ne!(third, second);
    assert_eq!(
        mints(&harness.trace, agent).len(),
        1,
        "and still one: a third launch in the same run mints nothing"
    );

    let boundary = harness.trace.len();
    harness.launch = true;
    let recovery_from = harness.trace.len();
    harness.restart();
    assert!(
        harness.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "restart alone does not repeat interrupted goal work"
    );
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert!(
        harness.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "plain Resume does not supply recovery consent"
    );
    let expected = harness
        .supervisor
        .recovery_opportunity(agent)
        .unwrap()
        .expected;
    let handled = harness.recover(RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    });
    assert_eq!(handled.recovery().expect("command progress").refusal, None);
    assert!(harness.journal.iter().any(|entry| matches!(&entry.record,
        Record::RecoveryAuthorized(authorization) if authorization.request_id == Hex32::new([41; 32]))));

    let run_two = mints(&harness.trace[boundary..], agent);
    assert_eq!(
        run_two.len(),
        1,
        "the second daemon run mints again for the same live agent"
    );
    assert_ne!(
        run_two[0], run_one[0],
        "corroboration, not the proof: what shows the second run *drew* is `run_two.len() == 1` \
         above, because replay emits no `Persist` at all. This inequality is carried by \
         `FakeRng`'s counter landing elsewhere after `restart()`"
    );
    assert_eq!(
        mints(&harness.trace, agent),
        vec![run_one[0].clone(), run_two[0].clone()],
        "both hashes stand: validity is set membership, and nothing prunes the first"
    );
    assert!(
        !launch_positions(&harness.trace[boundary..]).is_empty(),
        "a control: the re-issued turn did launch, so the second mint armed something"
    );
}

// ---- the sixth: which token arms the child ----------------------------------------------------

/// Drives the machine while **proving** the caller honours the ordering contract
/// [`Input::Persisted`] states: every acknowledgement is the oldest outstanding persist, so the
/// acknowledged sequence is always a contiguous prefix of the emitted one, in emission order.
///
/// **The driver is the reusable half of the test below.** `Harness::drive` also happens to
/// acknowledge in FIFO order, but as an implementation detail nothing asserts — so a test that
/// reaches a hazard through `drive` can be answered with "your driver acknowledged out of order".
/// Here it cannot: the prefix property is checked before every acknowledgement, and a violation
/// fails the driver rather than the subject.
struct InOrder {
    harness: Harness,
    emitted: Vec<PersistToken>,
    acked: Vec<PersistToken>,
    lsn: u64,
}

impl InOrder {
    fn new(harness: Harness) -> Self {
        Self {
            harness,
            emitted: Vec::new(),
            acked: Vec::new(),
            // Well past anything `Harness::drive` consumed before the driver took over, so an
            // acknowledgement here never reuses an `lsn` the harness already handed out.
            lsn: 1_000,
        }
    }

    fn record(&mut self, seen: &[Seen]) {
        for s in seen {
            if let Seen::Persist { token, .. } = s {
                self.emitted.push(*token);
            }
        }
        assert!(
            self.emitted.windows(2).all(|w| w[0] < w[1]),
            "a control on the driver: emission order is ascending `PersistToken`, so a contiguous \
             prefix of `emitted` really is 'the oldest outstanding first'"
        );
    }

    fn step(&mut self, input: Input) -> Vec<Seen> {
        let seen = self.harness.step(input);
        self.record(&seen);
        seen
    }

    /// Acknowledge the next outstanding persist — the only move the contract allows.
    fn ack_next(&mut self) -> Vec<Seen> {
        let token = self.emitted[self.acked.len()];
        self.acked.push(token);
        assert_eq!(
            self.acked.as_slice(),
            &self.emitted[..self.acked.len()],
            "the acknowledged set is a contiguous prefix of the emitted set, in emission order \
             (the contract `Input::Persisted` states)"
        );
        self.lsn += 1;
        let lsn = Lsn(self.lsn);
        self.step(self.harness.persisted(token, lsn))
    }

    fn outstanding(&self) -> &[PersistToken] {
        &self.emitted[self.acked.len()..]
    }
}

/// With two draws outstanding for one agent, each acknowledgement arms its child
/// with the token whose `HookTokenMinted` is the one that just reached the disk — every
/// acknowledgement issued in strict emission order.
///
/// **The mint is emitted lazily, and that is the whole mechanism.** `push_open_pty` mints only
/// when `compose_pair` finds no live token, and `push_open_pty` is itself re-entered *from an
/// acknowledgement* (`Then::OpenPty`). So a record emitted **before** the mint is necessarily
/// still outstanding when the mint appears, and the contract *obliges* the caller to acknowledge
/// that predecessor next — which drives the second draw. An ordering contract bounds what may be
/// acknowledged **after** a record; it cannot protect a lazily-emitted record from its own
/// predecessors, so in-order acknowledgement produces this shape rather than avoiding it.
///
/// Against a `pending` map keyed by **agent** the second draw displaces the first, and the first
/// mint's acknowledgement promotes the **second** token — arming a live child with a credential
/// that is not in the journal. That is the named failure: on a crash in that window
/// every call the child makes resolves to `Refused{gate: token}` and looks exactly like a
/// permissions bug. Keyed by the **hash**, each acknowledgement promotes its own token.
#[test]
fn an_acknowledged_mint_arms_a_child_with_its_own_token() {
    let mut d = InOrder::new(Harness::new());
    let agent = d.harness.hire();
    d.step(confirmed_session(agent));
    d.emitted.clear();
    d.acked.clear();

    // Two `agent.attach` calls, neither acknowledged: two `Attached` in flight, no mint yet.
    d.step(Input::Command(attach_command(agent)));
    d.step(Input::Command(attach_command(agent)));
    assert_eq!(
        d.outstanding().len(),
        2,
        "a control: two `Attached` are in flight and nothing has been minted"
    );

    // Acknowledge the oldest outstanding: `Then::OpenPty` re-enters `push_open_pty`, which finds
    // no live token and mints. The second `Attached` is still outstanding by construction.
    let first = d.ack_next();
    let mint_one = mints(&first, agent);
    assert_eq!(mint_one.len(), 1, "a control: the first ack minted once");
    assert!(
        !first
            .iter()
            .any(|seen| matches!(seen, Seen::OpenPty { .. })),
        "a control: no PTY is opened before any mint has been acknowledged"
    );

    // Acknowledge the next oldest — again what the contract requires — and it draws a second time.
    let second = d.ack_next();
    let mint_two = mints(&second, agent);
    assert_eq!(mint_two.len(), 1, "a control: the second ack minted again");
    assert_ne!(
        mint_one[0], mint_two[0],
        "a control: two distinct draws are outstanding for one agent"
    );

    // Acknowledge the first mint. The second is still in flight.
    let released = d.ack_next();
    assert_eq!(
        d.outstanding().len(),
        1,
        "a control: exactly one persist is still in flight, and it is the second mint"
    );
    let spec = released
        .iter()
        .find_map(|seen| match seen {
            Seen::OpenPty { spec, .. } => Some(spec),
            _ => None,
        })
        .expect("a control: acknowledging a mint does release the launch parked under it");
    let handed = blake3_hex(token_of(spec).expect("the child was armed").as_bytes());

    assert_eq!(
        handed, mint_one[0],
        "the child is armed with the token whose record is on the disk, not with one still in \
         flight — every acknowledgement here was issued in strict emission order (handed \
         {handed}, on disk {}, in flight {})",
        mint_one[0], mint_two[0]
    );
}

// ---- the last three: the projection a presented hash resolves against -------------------------

/// Two confirmed draws for one agent both resolve, because the rule is
/// **set membership in the hashes journaled for a non-retired agent, never recency**.
///
/// The two draws are reached the way [`an_acknowledged_mint_arms_a_child_with_its_own_token`]
/// reaches them — the mint is emitted lazily from inside an earlier acknowledgement, so two
/// `agent.attach` calls in flight produce two draws in one run — and here **both** are
/// acknowledged, which is what puts both hashes on the disk.
///
/// **The contrast this test exists to hold is with `Mint`, one module away.** `Mint::live` is one
/// token per agent and a later confirmed draw *replaces* an earlier one there, because it holds
/// the **plaintext** a child is armed with and a child is armed with one. The projection a
/// *presented* token resolves against is the other table, and a fold that mirrored `Mint`'s —
/// `agent -> hash`, last write wins — passes every other test in this file while refusing every
/// call from the child armed under the first draw. That is the failure: after a crash only, for
/// adopted children only, looking exactly like a permissions bug.
///
/// **The controls are the two directions a stub would pass in.** A lookup answering `Some(agent)`
/// for everything is caught by the unminted hash; a lookup answering from the *emitted* record
/// rather than the acknowledged one is caught by the first hash not resolving until it is
/// acknowledged — which is also the durability requirement `Mint::pending` exists for, stated here over
/// the projection rather than over the plaintext.
#[test]
fn a_second_confirmed_draw_does_not_evict_the_first() {
    let mut d = InOrder::new(Harness::new());
    let agent = d.harness.hire();
    d.step(confirmed_session(agent));
    d.emitted.clear();
    d.acked.clear();

    d.step(Input::Command(attach_command(agent)));
    d.step(Input::Command(attach_command(agent)));

    // Each acknowledgement re-enters `push_open_pty`, which mints when it finds no live token.
    let first = d.ack_next();
    let second = d.ack_next();
    let (one, two) = (mints(&first, agent), mints(&second, agent));
    assert_eq!(one.len(), 1, "a control: the first ack minted once");
    assert_eq!(two.len(), 1, "a control: the second ack minted again");
    let (one, two) = (one[0].clone(), two[0].clone());
    assert_ne!(
        one, two,
        "a control: two distinct draws are outstanding for one agent"
    );
    assert_eq!(
        d.harness.supervisor.world().agent_for_token_hash(&one),
        None,
        "a control: a hash whose record is emitted and not acknowledged is not on the disk, and \
         the projection folds what reached the disk"
    );

    d.ack_next(); // the first mint
    assert_eq!(
        d.harness.supervisor.world().agent_for_token_hash(&one),
        Some(MintedFor::Live(agent)),
        "the acknowledged mint resolves to the agent it was minted for"
    );

    d.ack_next(); // the second mint
    assert_eq!(
        d.harness.supervisor.world().agent_for_token_hash(&two),
        Some(MintedFor::Live(agent)),
        "and so does the second draw"
    );
    assert_eq!(
        d.harness.supervisor.world().agent_for_token_hash(&one),
        Some(MintedFor::Live(agent)),
        "and the first is still there: validity is set membership, and a later draw evicts \
         nothing"
    );

    assert_eq!(
        d.harness
            .supervisor
            .world()
            .agent_for_token_hash(&blake3_hex(b"a token this daemon never minted")),
        None,
        "a control: a hash no `HookTokenMinted` carries resolves to no agent, so the assertions \
         above are not a lookup that answers `Some` for everything"
    );
}

/// A hash minted before a restart still resolves after replay, because `World` is a
/// projection and invariant 1 makes it one `dudes replay` rebuilds.
///
/// **This is the half a fold written in the wrong place fails.** The machine already sees this
/// record in `on_persisted` — that is where the run-local plaintext is promoted — and a projection
/// built there would answer correctly for the whole of run 1 and answer `None` for every hash
/// after a restart, because replay never enters `on_persisted`. The child that survives the
/// restart is exactly the child the token protects, so the failure would arrive only after a
/// crash, only for an adopted child.
///
/// **The controls, and why the second one is an absence.** The pre-restart lookup separates
/// "replay rebuilt it" from "it never worked". The second run then mints **nothing** — a token is
/// drawn at the first launch of a run and this restart resumes a spawned child rather than
/// launching one — so the hash that resolves afterwards cannot be this run's, which is a sharper
/// separation than an inequality of two hashes would be. That the second run replayed *anything*
/// is a positive control of its own: the fresh machine knows the agent only from `AgentHired`. And
/// the unminted hash stops a lookup answering `Some` for everything from passing.
///
/// The other half of token validity across a restart — run 1's hash and run 2's both valid at once —
/// is [`a_second_confirmed_draw_does_not_evict_the_first`] for the coexistence and
/// [`a_second_daemon_run_mints_again_for_a_live_agent`] for the second draw; this test is the
/// projection surviving replay, and does not restate either.
#[test]
fn a_hash_minted_before_a_restart_resolves_after_replay() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let run_one = mints(&harness.trace, agent);
    assert_eq!(run_one.len(), 1, "a control: run 1 minted once");
    let run_one = run_one[0].clone();
    assert_eq!(
        harness.supervisor.world().agent_for_token_hash(&run_one),
        Some(MintedFor::Live(agent)),
        "a control: the hash resolves in the run that minted it, so a failure after the restart \
         below is the replay and not the fold"
    );

    let boundary = harness.trace.len();
    harness.restart();

    assert!(
        harness.supervisor.world().agents.contains_key(&agent),
        "a control: the fresh machine knows this agent only because it replayed `AgentHired`, so \
         replay ran"
    );
    assert!(
        mints(&harness.trace[boundary..], agent).is_empty(),
        "a control: the second run drew nothing — it resumes a spawned child rather than \
         launching one, and a token is drawn at a launch — so whatever resolves below is the \
         replayed hash and not this run's"
    );

    assert_eq!(
        harness.supervisor.world().agent_for_token_hash(&run_one),
        Some(MintedFor::Live(agent)),
        "the fresh machine folded the previous run's `HookTokenMinted` out of the journal: the \
         plaintext died with that daemon, the hash did not"
    );
    assert_eq!(
        harness
            .supervisor
            .world()
            .agent_for_token_hash(&blake3_hex(b"a token this daemon never minted")),
        None,
        "a control: an unminted hash still resolves to no agent after a replay"
    );
}

fn end_the_killed_turn(harness: &mut Harness, agent: AgentId) {
    let turn = harness
        .open_turn(agent)
        .expect("a control: the turn child outlives the retirement until its kill lands");
    harness.drive(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: None,
            signal: Some(9),
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::NoResult,
        last_message: None,
    });
}

/// A retired agent with a recorded child keeps its token until that child's end is recorded.
///
/// The map itself is never pruned: retirement is read from `AgentRow::retired`, the fact
/// `AgentRetired` already writes, rather than folded a second time into a second place. Two folds
/// of one journaled fact are two things that can disagree, and the one that would be wrong is the
/// one nothing replays.
#[test]
fn a_retired_agents_hash_names_it_until_its_last_child_ends() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let hash = mints(&harness.trace, agent);
    assert_eq!(hash.len(), 1, "a control: the run minted once");
    let hash = hash[0].clone();
    assert_eq!(
        harness.supervisor.world().agent_for_token_hash(&hash),
        Some(MintedFor::Live(agent)),
        "a control: it resolves while the agent is live, so the retirement below is what changes \
         the answer"
    );

    harness.drive(Input::Command(Command::Retire {
        agent,
        reason: "the quarter ended".to_owned(),
        browser_retire: None,
    }));
    assert!(
        harness.supervisor.world().agents[&agent].retired,
        "a control: the retirement reached the disk and was folded"
    );

    assert_eq!(
        harness.supervisor.world().agent_for_token_hash(&hash),
        Some(MintedFor::RetiredWithChild(agent)),
        "while its turn child runs, a retired agent's hash reaches the retired-agent row"
    );

    end_the_killed_turn(&mut harness, agent);
    assert_eq!(
        harness.supervisor.world().agent_for_token_hash(&hash),
        None,
        "once the child's end is recorded, the hash names no agent"
    );
}

/// The whole-domain projection answers exactly what the per-hash one answers, for
/// every hash the fold holds — so a reader on another thread that takes the map is reading the
/// same rule and not a second copy of it.
///
/// **Both directions, because one alone is satisfiable by a mistake.** A map that carried every
/// key of the fold would pass an "is present" check while honouring a retired agent's hash; an
/// empty map would pass an "is absent" check. So each state asserts the agreement over the map's
/// own keys *and* over a hash the map does not carry, and the live state is what makes the
/// retired state evidence rather than an empty map.
#[test]
fn the_published_domain_answers_what_the_per_hash_lookup_answers() {
    /// The two doors agree about `hash`, whatever they answer.
    fn agrees(harness: &Harness, hash: &str) {
        assert_eq!(
            harness
                .supervisor
                .world()
                .token_hash_owners()
                .get(hash)
                .copied(),
            harness.supervisor.world().agent_for_token_hash(hash),
            "the two doors must not disagree about {hash}"
        );
    }

    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let hash = mints(&harness.trace, agent);
    assert_eq!(hash.len(), 1, "a control: the run minted once");
    let hash = hash[0].clone();
    let unminted = blake3_hex(b"a token this daemon never minted");

    assert_eq!(
        harness.supervisor.world().token_hash_owners().get(&hash),
        Some(&MintedFor::Live(agent)),
        "a control: the live agent's hash is in the domain, so the absence below is the \
         retirement and not an empty map"
    );
    assert_eq!(
        harness
            .supervisor
            .world()
            .token_hash_owners()
            .get(&unminted),
        None,
        "a control: the map is a lookup and not something that answers for every key"
    );
    for hash in harness
        .supervisor
        .world()
        .token_hash_owners()
        .keys()
        .cloned()
        .chain([hash.clone(), unminted.clone()])
    {
        agrees(&harness, &hash);
    }

    harness.drive(Input::Command(Command::Retire {
        agent,
        reason: "the quarter ended".to_owned(),
        browser_retire: None,
    }));

    assert_eq!(
        harness.supervisor.world().token_hash_owners().get(&hash),
        Some(&MintedFor::RetiredWithChild(agent)),
        "a retired agent's hash stays in the domain while its turn child runs"
    );
    for hash in [hash.clone(), unminted.clone()] {
        agrees(&harness, &hash);
    }

    end_the_killed_turn(&mut harness, agent);
    assert_eq!(
        harness.supervisor.world().token_hash_owners().get(&hash),
        None,
        "and leaves it once that child's end is recorded, because the domain is the per-hash domain"
    );
    for hash in [hash, unminted] {
        agrees(&harness, &hash);
    }
}
