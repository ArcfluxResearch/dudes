//! A browser retirement's attribution: the machine journals it on the
//! `AgentRetired` it was handed, and the fold reads that record exactly as a local one.

use std::path::PathBuf;

use dudes_proto::record::{BrowserRetire, Hex32};
use dudes_proto::{AgentBudget, Entry, Record};
use dudes_supervisor::testing::Harness;
use dudes_supervisor::{Command, Input, World};
use dudes_types::{AgentId, Lsn, Provider, Timestamp, Usd};

fn attribution() -> BrowserRetire {
    BrowserRetire {
        request_id: Hex32::new([1; 32]),
        request_digest: Hex32::new([2; 32]),
        session_digest: Hex32::new([3; 32]),
    }
}

/// The arm journals the attribution it was handed beside the fixed reason, and the agent folds
/// as retired.
#[test]
fn the_retire_arm_journals_the_attribution_it_is_handed() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness.drive(Input::Command(Command::Retire {
        agent,
        reason: "retired from the Floor".into(),
        browser_retire: Some(attribution()),
    }));
    let retired: Vec<_> = harness
        .journal
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::AgentRetired {
                id,
                reason,
                browser_retire,
            } => Some((*id, reason.clone(), browser_retire.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        retired,
        [(
            agent,
            "retired from the Floor".to_owned(),
            Some(attribution())
        )]
    );
    assert!(harness.supervisor.world().agents[&agent].retired);
}

fn fold(browser_retire: Option<BrowserRetire>) -> World {
    let agent = AgentId::from_parts(1, 7);
    let at = Timestamp::from_unix_millis(1_787_394_800_000);
    let mut world = World::default();
    let hire = Record::AgentHired {
        character: None,
        accent: None,
        id: agent,
        name: "Hector".into(),
        role: "infra".into(),
        department: "infra".into(),
        provider: Provider::Claude,
        cwd: PathBuf::from("/work/repo"),
        budget: AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        model: None,
        trusted: false,
        workspace: None,
        worktree: None,
        mcp_servers: Vec::new(),
        browser_onboarding: None,
    };
    world.apply(Lsn(1), &Entry { at, record: hire });
    world.apply(
        Lsn(2),
        &Entry {
            at,
            record: Record::AgentRetired {
                id: agent,
                reason: "done".into(),
                browser_retire,
            },
        },
    );
    world
}

/// The fold has no arm for the attribution: the world a browser `AgentRetired` folds to is the
/// world the same record without it folds to.
#[test]
fn an_agent_retired_with_browser_retire_folds_exactly_as_one_without_it() {
    let local = fold(None);
    let browser = fold(Some(attribution()));
    assert!(
        local.agents.values().all(|row| row.retired),
        "control: the retirement folded"
    );
    assert_eq!(format!("{local:?}"), format!("{browser:?}"));
}
