//! The supervisor appends a move only when the history before it allows it, and a moved agent's
//! goal-less chat turn runs in the place it moved to.

use std::path::PathBuf;

use dudes_proto::placement::AgentMove;
use dudes_proto::record::{Hex32, RecordedWorkspace};
use dudes_proto::{AgentBudget, BrowserNoteAttribution, Record};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{Command, Input, RepositoryAdmission};
use dudes_types::{AgentId, Provider};

fn workspace(id: &str) -> RecordedWorkspace {
    RecordedWorkspace {
        id: id.into(),
        label: id.into(),
    }
}

fn hire(harness: &mut Harness) -> AgentId {
    harness.drive(Input::Command(Command::Hire {
        character: None,
        accent: None,
        name: "Hector".into(),
        role: "infra".into(),
        department: "infra".into(),
        provider: Provider::Claude,
        cwd: PathBuf::from("/repo/service"),
        workspace: Some(workspace("service")),
        budget: AgentBudget {
            usd_per_day: None,
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        model: None,
        trusted: false,
        worktree: None,
        mcp_servers: Vec::new(),
    }));
    *harness
        .supervisor
        .world()
        .agents
        .keys()
        .next_back()
        .expect("the hire was journaled")
}

fn admit(harness: &mut Harness, agent: AgentId) {
    let hired = harness.supervisor.world().agents[&agent].mcp.hire_lsn;
    harness
        .supervisor
        .observe_repository_admission(RepositoryAdmission::new([(agent, hired, true)]));
}

fn to(project: &str, cwd: &str) -> AgentMove {
    AgentMove {
        cwd: cwd.into(),
        workspace: workspace(project),
        worktree: None,
        budget: AgentBudget {
            usd_per_day: None,
            tokens_per_day: None,
        },
    }
}

fn moves(harness: &Harness) -> usize {
    harness
        .journal
        .iter()
        .filter(|entry| matches!(entry.record, Record::AgentMoved { .. }))
        .count()
}

#[test]
fn a_move_the_history_forbids_is_never_appended() {
    let mut harness = Harness::new();
    let agent = hire(&mut harness);
    let hired = harness.supervisor.world().agents[&agent].mcp.hire_lsn;

    harness.drive(Input::Command(Command::MoveAgent {
        agent,
        to: Box::new(to("service", "/repo/service")),
    }));
    assert_eq!(moves(&harness), 0, "the project it is already in");

    harness.drive(Input::Command(Command::MoveAgent {
        agent,
        to: Box::new(AgentMove {
            worktree: Some(dudes_types::WorktreeId::from_bytes([7; 16])),
            ..to("billing", "/repo/billing")
        }),
    }));
    assert_eq!(moves(&harness), 0, "a copy the agent does not hold");

    harness.set_goal(agent, &["true"]);
    harness.drive(Input::Command(Command::MoveAgent {
        agent,
        to: Box::new(to("billing", "/repo/billing")),
    }));
    assert_eq!(moves(&harness), 0, "an unfinished goal");
    assert_eq!(
        harness.supervisor.world().agents[&agent].mcp.hire_lsn,
        hired
    );
}

#[test]
fn an_idle_agent_moves_and_its_chat_turn_runs_in_the_new_folder() {
    let mut harness = Harness::new();
    let agent = hire(&mut harness);
    harness.drive(Input::Command(Command::MoveAgent {
        agent,
        to: Box::new(to("billing", "/repo/billing")),
    }));
    assert_eq!(moves(&harness), 1);
    admit(&mut harness, agent);
    harness.drive(Input::Spend {
        agent,
        view: dudes_supervisor::input::SpendView::default(),
    });
    let context_lsn = harness.supervisor.world().agents[&agent].context_lsn;
    harness.drive(Input::Command(Command::ConversationMessageQueued {
        agent,
        text: "Where are you?".to_owned(),
        browser: BrowserNoteAttribution {
            request_id_digest: Hex32::new([1; 32]),
            request_digest: Hex32::new([2; 32]),
            session_digest: Hex32::new([3; 32]),
        },
        context_lsn,
        context_reset_turn: None,
    }));
    let spawned: Vec<_> = harness
        .trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::Spawn { spec, .. } => Some(spec.cwd.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(spawned, [PathBuf::from("/repo/billing")]);
}
