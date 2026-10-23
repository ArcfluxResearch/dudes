//! The **producer** half of repository trust: a hire that declares no trust journals an untrusted agent.
//!
//! Two tests cover this, and they are not substitutes. One is its replay arm: an entry with the
//! field absent resolves to untrusted. That arm lives in
//! `dudes-journal/tests/legacy_agent_hired.rs`; this file holds the other. The replay arm governs what an **old** journal means. This one governs
//! what a **new** journal says, and only the daemon can get that wrong.
//!
//! **Why a defaulted field needs a test on the value the daemon writes.** `trusted` has no reader
//! yet, so nothing downstream fails if the wrong value is journaled — and under invariant 1 the
//! journal is permanent: a hire recorded `trusted: true` replays trusted forever, on every later
//! start, against every projection. When trust is wired to `setting_sources`, that value
//! decides whether a freshly cloned repository's command-running keys execute as the operator —
//! the exposure at stake. Without this file one line can be wrong with nothing to notice.
//!
//! **Reachability is not assertion.** Changing the hire arm to `trusted: true` leaves a suite that
//! never reads the field green, while an `assert!(false)` in the same `trusted:` initializer would
//! redden most of it. The adjacent field of this same record has the same shape: an inserted
//! `request.sandbox = Some(false)` passes every test in `dudes-rpc` that does not read it.
//!
//! **What makes the assertion discriminate.** It is taken from `Harness::journal` — the entries
//! that reached the disk, which is what a restart replays — not from a projection that could be
//! rebuilt differently, and not from the effect trace. It names the record by the values this
//! hire commanded, so a default-constructed or transposed `AgentHired` fails it rather than
//! satisfying a lone `!trusted`. And the count is asserted, so a hire journaled twice or never
//! fails here instead of passing a filter over an empty log.
//!
//! **The trusted arm** is `a_declared_hire_replays_trusted_and_an_undeclared_one_untrusted`,
//! beside this one. Its undeclared case is the control that stops the declared case
//! passing on a hard-wired `true`.

use std::path::{Path, PathBuf};

use dudes_proto::{AgentBudget, Record};
use dudes_supervisor::testing::Harness;
use dudes_supervisor::{Command, Input};
use dudes_types::{Provider, Usd};

/// A freshly cloned repository: any path at all.
const CWD: &str = "/work/freshly-cloned";
const NAME: &str = "Hector";

/// A hire without trust journals an untrusted agent.
///
/// Changing `trusted: false` at `machine.rs`'s `Command::Hire` arm to `true` makes
/// this test red.
#[test]
fn hire_without_trust_records_untrusted() {
    let mut harness = Harness::new();
    let budget = AgentBudget {
        usd_per_day: Some(Usd::from_dollars(10)),
        tokens_per_day: None,
    };
    harness.drive(Input::Command(Command::Hire {
        character: None,
        accent: None,
        name: NAME.into(),
        role: "infra".into(),
        department: "infra".into(),
        provider: Provider::Claude,
        cwd: PathBuf::from(CWD),
        workspace: None,
        budget,
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        model: None,
        trusted: false,
        worktree: None,
        mcp_servers: Vec::new(),
    }));

    let hires: Vec<&Record> = harness
        .journal
        .iter()
        .map(|entry| &entry.record)
        .filter(|record| matches!(record, Record::AgentHired { .. }))
        .collect();
    assert_eq!(
        hires.len(),
        1,
        "one hire, one durable record: {:?}",
        harness.journal
    );

    match hires[0] {
        Record::AgentHired {
            trusted,
            name,
            cwd,
            budget: recorded,
            ..
        } => {
            // The three below say this is *this* hire's record, so `!trusted` is a fact about the
            // value the machine wrote and not one about some other or default-built record.
            assert_eq!(name, NAME, "the record is the one this command produced");
            assert_eq!(
                cwd,
                Path::new(CWD),
                "the record is the one this command produced"
            );
            assert_eq!(
                *recorded, budget,
                "the record is the one this command produced"
            );
            assert!(
                !*trusted,
                "trust is declared at the hire, and a hire that declares none is \
                 untrusted. This value is durable — a `true` here replays trusted forever, and \
                 once trust is wired to setting sources it is what admits a cloned repository's command-running settings keys."
            );
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// Hire `NAME` in `CWD`, declaring `trusted` or not.
fn hire_declaring(trusted: bool) -> Harness {
    let mut harness = Harness::new();
    harness.drive(Input::Command(Command::Hire {
        character: None,
        accent: None,
        name: NAME.into(),
        role: "infra".into(),
        department: "infra".into(),
        provider: Provider::Claude,
        cwd: PathBuf::from(CWD),
        workspace: None,
        budget: AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        model: None,
        trusted,
        worktree: None,
        mcp_servers: Vec::new(),
    }));
    harness
}

/// What the journal says about the one hire, then what a restarted daemon rebuilds from it.
fn journaled_and_replayed_trust(harness: &mut Harness) -> (bool, bool) {
    let recorded: Vec<bool> = harness
        .records()
        .filter_map(|record| match record {
            Record::AgentHired { trusted, .. } => Some(*trusted),
            _ => None,
        })
        .collect();
    assert_eq!(recorded.len(), 1, "one hire, one durable record");
    harness.restart();
    let world = harness.supervisor.world();
    assert_eq!(world.agents.len(), 1, "the replay rebuilt the one agent");
    (
        recorded[0],
        world.agents.values().next().expect("one agent").trusted,
    )
}

/// A hire that declares trust is journaled trusted and replays trusted; a hire that
/// does not replays untrusted. The pair makes the first arm a claim about the declaration rather
/// than about a constant.
#[test]
fn a_declared_hire_replays_trusted_and_an_undeclared_one_untrusted() {
    assert_eq!(
        journaled_and_replayed_trust(&mut hire_declaring(true)),
        (true, true),
        "the declaration is recorded on AgentHired and is what replay yields"
    );
    assert_eq!(
        journaled_and_replayed_trust(&mut hire_declaring(false)),
        (false, false),
        "absence means untrusted, before and after a restart"
    );
}
