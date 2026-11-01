//! A recovery binding names the agent that issued the turn, not the agent that holds the goal now.

use dudes_proto::recovery::{RecoveryBinding, RecoveryWork};
use dudes_proto::recovery_state::RecoveryStateError;
use dudes_proto::{Entry, Goal, GoalBudget, Predicate, Record};
use dudes_supervisor::world::World;
use dudes_types::{AgentId, GoalId, Lsn, Mode, Provider, Timestamp, TurnId};

fn apply(world: &mut World, at: u64, record: Record) {
    let entry = Entry {
        at: Timestamp::from_unix_millis(0),
        record,
    };
    world.apply_checked(Lsn(at), &entry).unwrap();
}

fn hire(id: AgentId) -> Record {
    Record::AgentHired {
        id,
        name: "Hector".into(),
        role: "infra".into(),
        department: "infra".into(),
        provider: Provider::Claude,
        character: None,
        accent: None,
        cwd: "/repo/service".into(),
        budget: dudes_proto::AgentBudget {
            usd_per_day: None,
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        model: None,
        trusted: true,
        worktree: None,
        workspace: None,
        mcp_servers: Vec::new(),
        browser_onboarding: None,
    }
}

#[test]
fn a_turn_issued_by_the_goals_old_holder_is_not_bound_to_its_new_holder() {
    let (first, second) = (AgentId::from_parts(1, 1), AgentId::from_parts(1, 2));
    let goal = GoalId::from_parts(1, 1);
    let turn = TurnId::from_parts(1, 1);
    let mut world = World::default();
    apply(&mut world, 1, hire(first));
    apply(&mut world, 2, hire(second));
    apply(
        &mut world,
        3,
        Record::GoalSet {
            agent: first,
            goal: Goal {
                id: goal,
                text: "work".into(),
                predicate: Predicate::Command {
                    argv: vec!["/usr/bin/true".into()],
                    timeout_s: 1,
                },
                completion_check: None,
                baseline: None,
                budget: GoalBudget {
                    usd: None,
                    tokens: None,
                },
                hop_cap: 4,
            },
            browser_onboarding: None,
            browser_goal: None,
            goal_replacement: None,
        },
    );
    apply(
        &mut world,
        4,
        Record::TurnIssued {
            agent: first,
            turn,
            goal,
            task: "work".into(),
            session: None,
            mode: Mode::Managed,
            model_selection: None,
            lsn_of_decision: Lsn(3),
            answers: Vec::new(),
        },
    );
    apply(
        &mut world,
        5,
        Record::GoalReassigned {
            goal,
            from: first,
            to: second,
            reason: None,
        },
    );

    let binding = |holder| RecoveryBinding {
        work: RecoveryWork {
            agent: holder,
            goal,
            turn,
        },
        issue_lsn: world.turns[&turn].issue_lsn,
        hired_lsn: Lsn(1),
        worktree: None,
        provider: world.agents[&holder].provider,
        model_selection: None,
        ticket: None,
    };
    assert_eq!(
        world.recovery_facts(&binding(second)).unwrap_err(),
        RecoveryStateError::Binding
    );
}
