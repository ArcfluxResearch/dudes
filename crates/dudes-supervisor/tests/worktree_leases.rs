//! the lease table the supervisor folds from the journal.
//!
//! A lease is pending until an `AgentHired` names it, active while its agent lives or a recorded
//! child of that agent remains, released once both are gone, and removed by a `WorktreeRemoved`.
//! The fold reads recorded values only, and every state is the fold of records, so a restart
//! rebuilds the same table.

use std::collections::BTreeMap;
use std::path::PathBuf;

use dudes_proto::record::Hex32;
use dudes_proto::recovery::RecoveryAttribution;
use dudes_proto::{AgentBudget, ExitStatus, Record, TurnOutcome, WorktreeRemoval};
use dudes_supervisor::input::{RecoveryChoice, RecoveryCommand, RecoveryExpected};
use dudes_supervisor::testing::Harness;
use dudes_supervisor::{Command, Input, WorktreeState};
use dudes_types::{AgentId, GitCommit, Provider, Usd, WorktreeId};

const BASE: &str = "0123456789abcdef0123456789abcdef01234567";

fn lease(harness: &mut Harness, from: Option<WorktreeId>) -> WorktreeId {
    let before: Vec<WorktreeId> = harness
        .supervisor
        .world()
        .worktrees
        .keys()
        .copied()
        .collect();
    harness.drive(Input::Command(Command::LeaseWorktree {
        origin: PathBuf::from("/repo"),
        base: GitCommit::parse(BASE).unwrap(),
        from,
    }));
    let world = harness.supervisor.world();
    let gained: Vec<WorktreeId> = world
        .worktrees
        .keys()
        .copied()
        .filter(|id| !before.contains(id))
        .collect();
    assert_eq!(gained.len(), 1, "the lease was folded");
    gained[0]
}

fn hire_into(harness: &mut Harness, worktree: Option<WorktreeId>) -> AgentId {
    let before = harness.supervisor.world().agents.len();
    harness.drive(Input::Command(Command::Hire {
        character: None,
        accent: None,
        name: "Hector".into(),
        role: "infra".into(),
        department: "infra".into(),
        provider: Provider::Claude,
        cwd: PathBuf::from("/data/worktrees/tree"),
        workspace: None,
        budget: AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        trusted: true,
        worktree,
        mcp_servers: Vec::new(),
    }));
    assert_eq!(harness.supervisor.world().agents.len(), before + 1);
    *harness
        .supervisor
        .world()
        .agents
        .keys()
        .max_by_key(|agent| harness.supervisor.world().agents[agent].context_lsn)
        .expect("a hire was folded")
}

fn state(harness: &Harness, worktree: WorktreeId) -> Option<WorktreeState> {
    harness.supervisor.world().worktree_state(worktree)
}

fn leased_records(harness: &Harness) -> usize {
    harness
        .records()
        .filter(|record| matches!(record, Record::WorktreeLeased { .. }))
        .count()
}

fn removed(harness: &Harness) -> Vec<WorktreeRemoval> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::WorktreeRemoved { how, .. } => Some(*how),
            _ => None,
        })
        .collect()
}

fn retire(harness: &mut Harness, agent: AgentId) {
    harness.drive(Input::Command(Command::Retire {
        agent,
        reason: "done".into(),
        browser_retire: None,
    }));
}

#[test]
fn a_lease_moves_from_pending_to_active_to_released_to_removed() {
    let mut harness = Harness::new();
    assert!(
        harness.supervisor.world().worktrees.is_empty(),
        "a control: nothing is leased yet"
    );

    let tree = lease(&mut harness, None);
    assert_eq!(state(&harness, tree), Some(WorktreeState::Pending));
    assert_eq!(harness.supervisor.world().live_worktrees().count(), 1);

    let agent = hire_into(&mut harness, Some(tree));
    assert_eq!(state(&harness, tree), Some(WorktreeState::Active));
    assert_eq!(
        harness.supervisor.world().agents[&agent].worktree,
        Some(tree)
    );
    assert_eq!(
        harness.supervisor.world().worktrees[&tree].agent,
        Some(agent)
    );

    retire(&mut harness, agent);
    assert_eq!(state(&harness, tree), Some(WorktreeState::Released));
    assert_eq!(
        harness.supervisor.world().live_worktrees().count(),
        0,
        "a released tree is no longer one a hire may not overlap"
    );

    harness.drive(Input::Command(Command::RemoveWorktree {
        worktree: tree,
        how: WorktreeRemoval::Operator,
    }));
    assert_eq!(state(&harness, tree), Some(WorktreeState::Removed));
    assert_eq!(removed(&harness), [WorktreeRemoval::Operator]);
}

#[test]
fn a_retired_agent_with_a_recorded_child_keeps_its_lease_active() {
    let mut harness = Harness::new();
    let tree = lease(&mut harness, None);
    let agent = hire_into(&mut harness, Some(tree));
    harness.set_goal(agent, &["npm", "test"]);
    let turn = harness
        .open_turn(agent)
        .expect("a control: the goal started a turn whose child is recorded");

    retire(&mut harness, agent);
    assert!(harness.supervisor.world().agents[&agent].retired);
    assert_eq!(
        state(&harness, tree),
        Some(WorktreeState::Active),
        "the retired agent's turn child is still recorded, so the tree is still in use"
    );
    assert_eq!(harness.supervisor.world().live_worktrees().count(), 1);

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
    assert_eq!(
        state(&harness, tree),
        Some(WorktreeState::Released),
        "the end of the last child is what releases it"
    );
}

#[test]
fn a_removal_is_appended_only_when_the_lease_state_allows_it() {
    let mut harness = Harness::new();
    let remove = |harness: &mut Harness, tree, how| {
        harness.drive(Input::Command(Command::RemoveWorktree {
            worktree: tree,
            how,
        }));
    };

    let unknown = WorktreeId::from_bytes([3; 16]);
    remove(&mut harness, unknown, WorktreeRemoval::NeverAssigned);
    remove(&mut harness, unknown, WorktreeRemoval::Operator);
    assert!(removed(&harness).is_empty(), "no lease exists yet");

    let tree = lease(&mut harness, None);
    remove(&mut harness, tree, WorktreeRemoval::Operator);
    assert!(
        removed(&harness).is_empty(),
        "the operator removes a released tree, and a pending one is not released"
    );

    let agent = hire_into(&mut harness, Some(tree));
    remove(&mut harness, tree, WorktreeRemoval::NeverAssigned);
    remove(&mut harness, tree, WorktreeRemoval::Operator);
    assert!(
        removed(&harness).is_empty(),
        "a tree an agent runs in is removed by neither path"
    );

    retire(&mut harness, agent);
    remove(&mut harness, tree, WorktreeRemoval::NeverAssigned);
    assert!(
        removed(&harness).is_empty(),
        "a tree an agent ran in is never `NeverAssigned`, even after it retires"
    );
    remove(&mut harness, tree, WorktreeRemoval::Operator);
    remove(&mut harness, tree, WorktreeRemoval::Operator);
    assert_eq!(
        removed(&harness),
        [WorktreeRemoval::Operator],
        "a released tree is removed once"
    );
}

#[test]
fn a_pending_lease_is_removed_as_never_assigned() {
    let mut harness = Harness::new();
    let tree = lease(&mut harness, None);
    harness.drive(Input::Command(Command::RemoveWorktree {
        worktree: tree,
        how: WorktreeRemoval::NeverAssigned,
    }));
    assert_eq!(state(&harness, tree), Some(WorktreeState::Removed));
    assert_eq!(removed(&harness), [WorktreeRemoval::NeverAssigned]);
    assert_eq!(harness.supervisor.world().live_worktrees().count(), 0);
}

#[test]
fn each_lease_gets_its_own_identity_tree_and_branch_and_the_first_hire_owns_it() {
    let mut harness = Harness::new();
    let first_tree = lease(&mut harness, None);
    let other_tree = lease(&mut harness, Some(first_tree));
    assert_ne!(first_tree, other_tree);
    let row = &harness.supervisor.world().worktrees[&other_tree];
    assert_eq!(
        row.path,
        PathBuf::from(format!("/var/lib/dudes/worktrees/{other_tree}"))
    );
    assert_eq!(row.branch, format!("dudes/{}", other_tree.short()));
    assert_eq!(row.from, Some(first_tree));
    assert_eq!(leased_records(&harness), 2);

    let first = hire_into(&mut harness, Some(first_tree));
    let second = hire_into(&mut harness, Some(first_tree));
    assert_ne!(first, second);
    assert_eq!(
        harness.supervisor.world().worktrees[&first_tree].agent,
        Some(first),
        "the lease names the first agent hired into it"
    );
}

#[test]
fn a_restart_rebuilds_the_same_table_from_the_journal() {
    let mut harness = Harness::new();
    let first = lease(&mut harness, None);
    let second = lease(&mut harness, Some(first));
    let agent = hire_into(&mut harness, Some(first));
    retire(&mut harness, agent);
    harness.drive(Input::Command(Command::RemoveWorktree {
        worktree: second,
        how: WorktreeRemoval::NeverAssigned,
    }));
    let before = harness.supervisor.world().worktrees.clone();
    assert_eq!(before.len(), 2, "a control: the table is not empty");
    assert_eq!(before[&second].from, Some(first));

    harness.restart();

    assert_eq!(harness.supervisor.world().worktrees, before);
    assert_eq!(state(&harness, first), Some(WorktreeState::Released));
    assert_eq!(state(&harness, second), Some(WorktreeState::Removed));
}

#[test]
fn a_hire_that_names_no_lease_leaves_the_table_empty() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    assert_eq!(harness.supervisor.world().agents[&agent].worktree, None);
    assert!(harness.supervisor.world().worktrees.is_empty());
    assert_eq!(harness.supervisor.world().live_worktrees().count(), 0);
}

fn recover_available_tree(control: &mut Harness, control_agent: AgentId) -> RecoveryExpected {
    let recovery_from = control.trace.len();
    control.restart();
    assert!(
        control.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "restart alone does not repeat interrupted goal work"
    );
    control.drive(Input::Command(Command::Resume {
        agent: control_agent,
        browser: None,
    }));
    assert!(
        control.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "plain Resume does not supply recovery consent"
    );
    let expected = control
        .supervisor
        .recovery_opportunity(control_agent)
        .unwrap()
        .expected;
    let handled = control.recover(RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    });
    assert_eq!(handled.recovery().expect("command progress").refusal, None);
    assert!(control.journal.iter().any(|entry| matches!(&entry.record,
        Record::RecoveryAuthorized(authorization) if authorization.request_id == Hex32::new([41; 32]))));

    assert_eq!(
        control.trace[recovery_from..]
            .iter()
            .filter(|seen| matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. }))
            .count(),
        1,
        "a control: explicit recovery repeats the owed turn into the available tree"
    );
    expected
}

#[test]
fn a_missing_tree_holds_the_agent_before_the_first_decide_launches_into_it() {
    let scenario = || {
        let mut harness = Harness::new();
        harness.launch = false;
        let tree = lease(&mut harness, None);
        let agent = hire_into(&mut harness, Some(tree));
        harness.set_goal(agent, &["npm", "test"]);
        assert!(
            harness.open_turn(agent).is_some(),
            "a control: the goal issued a turn that has not spawned"
        );
        harness.launch = true;
        (harness, agent, tree)
    };
    let spawned = |harness: &Harness, from: usize| {
        harness.trace[from..]
            .iter()
            .filter(|seen| matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. }))
            .count()
    };
    let issued = |harness: &Harness| {
        harness
            .records()
            .filter(|record| matches!(record, Record::TurnIssued { .. }))
            .count()
    };

    let (mut control, control_agent, _) = scenario();
    let expected = recover_available_tree(&mut control, control_agent);

    let (mut harness, agent, tree) = scenario();
    let (before, turns) = (harness.trace.len(), issued(&harness));
    harness.restart_with_missing_worktrees(vec![agent]);
    assert_eq!(
        spawned(&harness, before),
        0,
        "no launch into a missing tree"
    );
    assert_eq!(issued(&harness), turns, "and no turn issued for it");
    let causes: Vec<dudes_proto::AutomaticHold> = harness
        .records()
        .filter_map(|record| match record {
            Record::AgentHeld {
                agent: held, cause, ..
            } if *held == agent => Some(cause.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        causes,
        [dudes_proto::AutomaticHold::WorktreeMissing { worktree: tree }]
    );
    assert_eq!(
        harness.supervisor.world().agents[&agent].automatic_hold,
        Some(dudes_proto::AutomaticHold::WorktreeMissing { worktree: tree })
    );
    assert_eq!(expected.work.agent, agent);
    assert_eq!(expected.work.turn, harness.open_turn(agent).unwrap());
    assert_eq!(
        harness
            .supervisor
            .recovery_opportunity(agent)
            .map(|opportunity| opportunity.expected),
        Err(dudes_supervisor::input::RecoveryRefusal::NotRecoverable)
    );
    let refused = harness.recover(RecoveryCommand {
        request_id: Hex32::new([43; 32]),
        digest: Hex32::new([44; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    });
    assert_eq!(
        refused.recovery().expect("command refusal").refusal,
        Some(dudes_supervisor::input::RecoveryRefusal::NotRecoverable)
    );
    assert!(
        harness
            .journal
            .iter()
            .all(|entry| !matches!(entry.record, Record::RecoveryAuthorized(_)))
    );
    assert_eq!(spawned(&harness, before), 0);
    assert_eq!(issued(&harness), turns);
}

/// The set the daemon reports is applied once, at start. An agent whose tree is restored and who is
/// resumed is not held again when its next turn ends.
#[test]
fn a_resumed_agent_is_not_held_again_at_its_next_turn_end() {
    let mut harness = Harness::new();
    harness.launch = false;
    let tree = lease(&mut harness, None);
    let agent = hire_into(&mut harness, Some(tree));
    harness.set_goal(agent, &["npm", "test"]);
    harness.launch = true;
    harness.restart_with_missing_worktrees(vec![agent]);
    let holds = |harness: &Harness| {
        harness
            .records()
            .filter(|record| matches!(record, Record::AgentHeld { .. }))
            .count()
    };
    assert_eq!(
        holds(&harness),
        1,
        "a control: the missing tree held the agent"
    );

    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert!(
        harness.host.spawned.is_empty(),
        "ordinary Resume does not repeat the owed goal turn"
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

    let turn = harness
        .open_turn(agent)
        .expect("resumed, the agent re-issues its turn");
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
        outcome: TurnOutcome::NoResult,
        last_message: None,
    });

    assert_eq!(
        holds(&harness),
        1,
        "and no second hold appeared at the turn end"
    );
    assert_eq!(
        harness.supervisor.world().agents[&agent].automatic_hold,
        None,
        "the agent is not held now"
    );
}
