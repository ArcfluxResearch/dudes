//! A browser goal change's attribution: the machine journals it on the `GoalSet` it
//! was handed, and the fold reads that `GoalSet` exactly as a local one.

use std::path::PathBuf;

use dudes_proto::record::{BrowserGoal, Hex32};
use dudes_proto::{AgentBudget, Entry, Goal, GoalBudget, LoopDecision, Predicate, Record, Spend};
use dudes_supervisor::testing::Harness;
use dudes_supervisor::{Command, Input, World};
use dudes_types::{AgentId, GoalId, Lsn, Provider, Timestamp, Usd};

fn attribution(replaced_goal: GoalId) -> BrowserGoal {
    BrowserGoal {
        kind: dudes_proto::record::BrowserGoalKind::Change,
        request_id: Hex32::new([1; 32]),
        request_digest: Hex32::new([2; 32]),
        replaced_goal,
        reserved: Spend {
            usd: Usd::from_micros(100_000),
            ..Spend::default()
        },
        session_digest: Hex32::new([3; 32]),
        check_chosen: false,
    }
}

/// The arm mints the new goal, journals the attribution it was handed beside it, and decides on
/// the new goal at once: a paused agent's next record is its `Hold`.
#[test]
fn the_goal_set_arm_journals_the_attribution_it_is_handed_and_decides() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness.drive(Input::Command(Command::Pause {
        agent,
        browser: None,
    }));
    let replaced = GoalId::from_parts(1, 1);
    harness.drive(Input::Command(Command::GoalSet {
        completion_check: None,
        agent,
        text: "ship the fix".into(),
        predicate: Predicate::Command {
            argv: vec!["/usr/bin/true".into()],
            timeout_s: 10,
        },
        budget: GoalBudget {
            usd: Some(Usd::from_micros(50_000)),
            tokens: None,
        },
        hop_cap: 3,
        goal_replacement: None,
        browser_goal: Some(Box::new(attribution(replaced))),
        baseline: None,
    }));
    let at = harness
        .journal
        .iter()
        .position(|entry| matches!(entry.record, Record::GoalSet { .. }))
        .expect("the GoalSet was journaled");
    let Record::GoalSet {
        agent: owner,
        goal,
        browser_onboarding,
        browser_goal,
        ..
    } = &harness.journal[at].record
    else {
        unreachable!()
    };
    assert_eq!(*owner, agent);
    assert_eq!(goal.text, "ship the fix");
    assert_eq!(goal.hop_cap, 3);
    assert!(browser_onboarding.is_none());
    assert_eq!(browser_goal.as_deref(), Some(&attribution(replaced)));
    assert!(
        matches!(
            &harness.journal[at + 1].record,
            Record::LoopDecided {
                agent: who,
                decision: LoopDecision::Hold { .. },
                ..
            } if *who == agent
        ),
        "{:?}",
        harness.journal[at + 1].record
    );
    assert_eq!(
        harness.supervisor.world().agents[&agent].goal,
        Some(goal.id)
    );
}

fn fold(browser_goal: Option<Box<BrowserGoal>>) -> World {
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
            record: Record::GoalSet {
                agent,
                goal: Goal {
                    completion_check: None,
                    id: GoalId::from_parts(2, 9),
                    text: "ship the fix".into(),
                    predicate: Predicate::Command {
                        argv: vec!["/usr/bin/true".into()],
                        timeout_s: 10,
                    },
                    budget: GoalBudget {
                        usd: Some(Usd::from_micros(50_000)),
                        tokens: None,
                    },
                    hop_cap: 3,
                    baseline: None,
                },
                browser_onboarding: None,
                goal_replacement: None,
                browser_goal,
            },
        },
    );
    world
}

/// The fold has no arm for the attribution: the world a browser `GoalSet` folds to is the world
/// the same record without it folds to, agent rows and goal rows alike.
#[test]
fn a_goal_set_with_browser_goal_folds_exactly_as_one_without_it() {
    let local = fold(None);
    let browser = fold(Some(Box::new(attribution(GoalId::from_parts(1, 1)))));
    assert_eq!(local.goals.len(), 1, "control: the goal folded");
    assert_eq!(format!("{local:?}"), format!("{browser:?}"));
}
