//! An idle agent moves to another project's place: what the fold keeps, restores and refuses.

use dudes_proto::placement::AgentMove;
use dudes_proto::record::RecordedWorkspace;
use dudes_proto::{AgentBudget, Entry, Record};
use dudes_supervisor::world::{MoveHistoryError, World};
use dudes_types::{AgentId, Lsn, SessionId, Timestamp, TurnId, Usd};

fn agent() -> AgentId {
    AgentId::from_bytes([1; 16])
}

fn lsn(offset: u64) -> Lsn {
    Lsn::from_parts(1, offset).unwrap()
}

fn budget(dollars: Option<u64>) -> AgentBudget {
    AgentBudget {
        usd_per_day: dollars.map(Usd::from_dollars),
        tokens_per_day: None,
    }
}

fn workspace(id: &str) -> RecordedWorkspace {
    RecordedWorkspace {
        id: id.into(),
        label: id.into(),
    }
}

fn hire(project: Option<&str>) -> Record {
    Record::AgentHired {
        id: agent(),
        name: "Hector".into(),
        role: "infra".into(),
        department: "infra".into(),
        provider: dudes_types::Provider::Claude,
        character: None,
        accent: None,
        cwd: "/repo/service".into(),
        budget: budget(Some(20)),
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        model: None,
        trusted: true,
        worktree: None,
        workspace: project.map(workspace),
        mcp_servers: Vec::new(),
        browser_onboarding: None,
    }
}

fn place(project: &str, cwd: &str, dollars: Option<u64>) -> AgentMove {
    AgentMove {
        cwd: cwd.into(),
        workspace: workspace(project),
        worktree: None,
        budget: budget(dollars),
    }
}

fn moved(to: AgentMove) -> Record {
    Record::AgentMoved { agent: agent(), to }
}

fn learn(session: &str) -> Record {
    Record::SessionLearned {
        agent: agent(),
        turn: TurnId::from_bytes([2; 16]),
        session: SessionId::new(session),
    }
}

fn fold(world: &mut World, at: u64, record: Record) -> Lsn {
    let position = lsn(at);
    let entry = Entry {
        at: Timestamp::from_unix_millis(0),
        record,
    };
    world.validate_move_record(&entry).unwrap();
    world.apply_checked(position, &entry).unwrap();
    position
}

fn refused(world: &World, record: Record) -> MoveHistoryError {
    world
        .validate_move_record(&Entry {
            at: Timestamp::from_unix_millis(0),
            record,
        })
        .unwrap_err()
}

fn hired_in_service() -> World {
    let mut world = World::default();
    fold(&mut world, 100, hire(Some("service")));
    world
}

#[test]
fn a_move_to_a_new_place_starts_fresh_and_untrusted() {
    let mut world = hired_in_service();
    let learned = fold(&mut world, 110, learn("service-session"));
    let at = fold(
        &mut world,
        120,
        moved(place("billing", "/repo/billing", None)),
    );

    let row = &world.agents[&agent()];
    let here = row.repositories.singleton().unwrap();
    assert_eq!(here.cwd(), std::path::Path::new("/repo/billing"));
    assert_eq!(here.workspace().unwrap().id, "billing");
    assert!(!row.trusted);
    assert_eq!((row.session.as_ref(), row.session_lsn), (None, None));
    assert_eq!(row.mcp.hire_lsn, at);

    let kept = &row.other_projects["service"];
    assert!(kept.trusted);
    assert_eq!(kept.session, Some(SessionId::new("service-session")));
    assert_eq!(kept.session_lsn, Some(learned));
}

#[test]
fn a_return_restores_the_stored_session_and_trust() {
    let mut world = hired_in_service();
    let learned = fold(&mut world, 110, learn("service-session"));
    fold(
        &mut world,
        120,
        moved(place("billing", "/repo/billing", None)),
    );
    fold(&mut world, 130, learn("billing-session"));
    let back = fold(
        &mut world,
        140,
        moved(place("service", "/repo/service", None)),
    );

    let row = &world.agents[&agent()];
    assert!(row.trusted);
    assert_eq!(row.session, Some(SessionId::new("service-session")));
    assert_eq!(row.session_lsn, Some(learned));
    assert_eq!(row.mcp.hire_lsn, back);
    assert_eq!(
        row.other_projects["billing"].session,
        Some(SessionId::new("billing-session"))
    );
    assert!(!row.other_projects.contains_key("service"));
}

#[test]
fn the_ceiling_follows_the_move_and_the_hire_ceiling_never_changes() {
    let mut world = hired_in_service();
    fold(
        &mut world,
        120,
        moved(place("billing", "/repo/billing", Some(5))),
    );
    let row = &world.agents[&agent()];
    assert_eq!(row.budget, budget(Some(5)));
    assert_eq!(row.hired_budget, budget(Some(20)));
    fold(
        &mut world,
        140,
        moved(place("service", "/repo/service", Some(20))),
    );
    let row = &world.agents[&agent()];
    assert_eq!(row.budget, budget(Some(20)));
    assert_eq!(row.hired_budget, budget(Some(20)));
}

#[test]
fn a_stored_shared_folder_with_another_folder_is_not_reused() {
    let mut world = hired_in_service();
    fold(&mut world, 110, learn("service-session"));
    fold(
        &mut world,
        120,
        moved(place("billing", "/repo/billing", None)),
    );
    fold(
        &mut world,
        140,
        moved(place("service", "/repo/service-moved", None)),
    );

    let row = &world.agents[&agent()];
    assert_eq!(
        row.repositories.singleton().unwrap().cwd(),
        std::path::Path::new("/repo/service-moved")
    );
    assert!(!row.trusted);
    assert_eq!((row.session.as_ref(), row.session_lsn), (None, None));
}

#[test]
fn an_impossible_move_is_refused_by_name() {
    let mut world = World::default();
    let to = || place("billing", "/repo/billing", None);
    assert_eq!(refused(&world, moved(to())), MoveHistoryError::UnknownAgent);

    fold(&mut world, 100, hire(Some("service")));
    assert_eq!(
        refused(&world, moved(place("service", "/repo/other", None))),
        MoveHistoryError::SameProject
    );

    fold(
        &mut world,
        200,
        serde_json::from_value(serde_json::json!({
            "t": "turn_issued", "agent": agent(), "turn": TurnId::from_bytes([3; 16]),
            "goal": "01ARZ3NDEKTSV4RRFFQ69G5FAX", "task": "run tests", "mode": "managed",
            "lsn_of_decision": 1
        }))
        .unwrap(),
    );
    assert_eq!(refused(&world, moved(to())), MoveHistoryError::Busy);

    fold(
        &mut world,
        300,
        Record::AgentRetired {
            id: agent(),
            reason: "done".into(),
            browser_retire: None,
        },
    );
    assert_eq!(refused(&world, moved(to())), MoveHistoryError::UnknownAgent);
}

#[test]
fn an_agent_hired_outside_a_project_or_with_an_unfinished_goal_does_not_move() {
    let mut world = World::default();
    fold(&mut world, 100, hire(None));
    assert_eq!(
        refused(&world, moved(place("billing", "/repo/billing", None))),
        MoveHistoryError::NoProject
    );

    let mut world = hired_in_service();
    fold(
        &mut world,
        150,
        serde_json::from_value(serde_json::json!({
            "t": "goal_set", "agent": agent(),
            "goal": {
                "id": dudes_types::GoalId::from_bytes([4; 16]), "text": "fix it",
                "predicate": {"command": {"argv": ["/usr/bin/true"], "timeout_s": 1}},
                "budget": {}, "hop_cap": 1
            }
        }))
        .unwrap(),
    );
    assert_eq!(
        refused(&world, moved(place("billing", "/repo/billing", None))),
        MoveHistoryError::Unfinished
    );
}

#[test]
fn a_move_cannot_name_an_own_copy_the_agent_does_not_hold() {
    let mut world = hired_in_service();
    let mut to = place("billing", "/repo/billing", None);
    to.worktree = Some(dudes_types::WorktreeId::from_bytes([9; 16]));
    assert_eq!(refused(&world, moved(to)), MoveHistoryError::ForeignCopy);
    fold(
        &mut world,
        120,
        moved(place("billing", "/repo/billing", None)),
    );
}

#[test]
fn an_undelivered_answer_stays_in_the_project_it_belongs_to() {
    let mut world = hired_in_service();
    fold(
        &mut world,
        110,
        Record::HumanQuestion {
            agent: agent(),
            text: "which branch?".into(),
        },
    );
    fold(
        &mut world,
        120,
        Record::HumanAnswer {
            agent: agent(),
            text: "main".into(),
            provenance: None,
            browser_answer: None,
        },
    );
    assert_eq!(
        refused(&world, moved(place("billing", "/repo/billing", None))),
        MoveHistoryError::UndeliveredAnswers
    );
}
