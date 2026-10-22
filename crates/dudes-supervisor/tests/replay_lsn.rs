//! The fold is told **where** the entry it is folding sits in the log.
//!
//! `Entry` is `at` plus the record and carries no position of its own — the frame header holds it
//! and the payload does not. So `World::apply` can only learn a position from its caller, and the
//! two callers are the two doors into the fold: `Input::Replayed`, which the replay driver feeds
//! from the log, and `Input::Persisted`, which the runtime feeds from the fsync it just
//! acknowledged. This file asserts that each door passes the position it holds rather than a
//! constant, which is what a later record named by the `lsn` that set it will rest on.
//!
//! Nothing here asserts a *decision* taken on the position. Nothing takes one yet; `decide()`
//! reads no position and invariant 5 says it never will.

use std::path::PathBuf;

use dudes_proto::{AgentBudget, Entry, Record};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{Command, Input};
use dudes_types::{AgentId, Lsn, Provider, SessionId, Timestamp, TurnId, Usd};

fn agent() -> AgentId {
    AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("an agent id")
}

/// The first of the two replayed entries.
fn hired() -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_000),
        record: Record::AgentHired {
            character: None,
            accent: None,
            id: agent(),
            name: "Hector".to_owned(),
            role: "infra".to_owned(),
            department: "infra".to_owned(),
            provider: Provider::Claude,
            cwd: PathBuf::from("/repo"),
            budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            trusted: false,
            workspace: None,
            worktree: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        },
    }
}

/// The second, which folds onto the agent the first created.
fn session_learned() -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_001),
        record: Record::SessionLearned {
            agent: agent(),
            turn: TurnId::NIL,
            session: SessionId::from("550e8400-e29b-41d4-a716-446655440000"),
        },
    }
}

fn hire_command() -> Command {
    Command::Hire {
        character: None,
        accent: None,
        name: "Hector".into(),
        role: "infra".into(),
        department: "infra".into(),
        provider: Provider::Claude,
        cwd: PathBuf::from("/repo"),
        workspace: None,
        budget: AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        trusted: false,
        worktree: None,
        mcp_servers: Vec::new(),
    }
}

/// Replay folds each entry at **its own** position, in log order.
///
/// Two entries and two reads rather than one of each: a single read cannot tell a threaded
/// position from a constant that happens to equal it, and reading between the two is what makes
/// the order part of the claim rather than the end state.
#[test]
fn replay_folds_each_entry_at_its_own_lsn() {
    let mut harness = Harness::new();

    assert_eq!(
        harness.supervisor.world().last_applied_lsn,
        None,
        "control: a world that has folded nothing reports no position, so a position read below \
         was put there by a fold and not by construction"
    );

    harness.drive(Input::Replayed {
        lsn: Lsn(1),
        entry: hired(),
    });
    assert_eq!(
        harness.supervisor.world().last_applied_lsn,
        Some(Lsn(1)),
        "the first entry — the hire — is folded at lsn 1, the position it was replayed from"
    );

    harness.drive(Input::Replayed {
        lsn: Lsn(2),
        entry: session_learned(),
    });
    assert_eq!(
        harness.supervisor.world().last_applied_lsn,
        Some(Lsn(2)),
        "the second entry — the session — is folded at lsn 2, so the fold advanced with the log \
         rather than standing still"
    );
}

/// The live door folds at the position its **acknowledgement** carried.
///
/// The sibling of the claim above: `on_persisted` has held the `lsn` all along and must hand it
/// to the same fold, or a record written live would sit at one position and the same record
/// replayed at another — and "replay decides what the live run decided" would quietly stop being
/// true for anything named by position.
#[test]
fn an_acknowledged_entry_is_folded_at_the_lsn_of_its_acknowledgement() {
    let mut harness = Harness::new();
    // Acknowledge by hand, at a position this test picks, rather than letting the harness hand
    // out the next one: a position the harness chose could equal whatever a broken fold stored.
    harness.fsync = false;

    let seen = harness.step(Input::Command(hire_command()));
    let [Seen::Persist { token, .. }] = seen.as_slice() else {
        panic!("one hire, one persist; saw {seen:?}");
    };
    let token = *token;

    assert_eq!(
        harness.supervisor.world().last_applied_lsn,
        None,
        "control: an emitted persist is not a fold — the entry is folded on its acknowledgement"
    );

    harness.step(harness.persisted(token, Lsn(7)));
    assert_eq!(
        harness.supervisor.world().last_applied_lsn,
        Some(Lsn(7)),
        "the hire is folded at lsn 7, the position its acknowledgement carried"
    );
}
