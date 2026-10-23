//! Desired-policy history and immutable physical launch grants during replay.

use dudes_proto::{AgentBudget, Entry, Record};
use dudes_supervisor::world::{McpHistoryError, World};
use dudes_types::{AgentId, Lsn, McpServerName, Provider, Timestamp, TurnId};

fn agent() -> AgentId {
    AgentId::from_bytes([1; 16])
}
fn turn() -> TurnId {
    TurnId::from_bytes([2; 16])
}
fn names(values: &[&str]) -> Vec<McpServerName> {
    values
        .iter()
        .map(|name| McpServerName::parse(name).unwrap())
        .collect()
}

fn fold(world: &mut World, offset: u64, record: Record) -> Lsn {
    let lsn = Lsn::from_parts(1, offset).unwrap();
    world.validate_mcp_record(lsn, &record).unwrap();
    world.apply(
        lsn,
        &Entry {
            at: Timestamp::from_unix_millis(0),
            record,
        },
    );
    lsn
}

fn hired() -> World {
    let mut world = World::default();
    fold(
        &mut world,
        100,
        Record::AgentHired {
            id: agent(),
            name: "Hector".into(),
            role: "infra".into(),
            department: "infra".into(),
            provider: Provider::Claude,
            character: None,
            accent: None,
            cwd: "/repo".into(),
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
            workspace: None,
            mcp_servers: names(&["a"]),
            browser_onboarding: None,
        },
    );
    world
}

fn issue(world: &mut World) {
    let record: Record = serde_json::from_value(serde_json::json!({
        "t": "turn_issued", "agent": agent(), "turn": turn(),
        "goal": "01ARZ3NDEKTSV4RRFFQ69G5FAX", "task": "run tests", "mode": "managed",
        "lsn_of_decision": 1
    }))
    .unwrap();
    fold(world, 200, record);
}

fn spawn(grant: Option<Lsn>, pid: u32) -> Record {
    Record::TurnSpawned {
        agent: agent(),
        turn: turn(),
        pid,
        start_time: 7,
        boot: Some("fixture-boot".into()),
        session: None,
        reissued: false,
        mcp_grant: grant,
    }
}

#[test]
fn replacement_changes_only_desired_names_and_activates_at_its_position() {
    let mut world = hired();
    fold(
        &mut world,
        150,
        Record::Pause {
            agent: agent(),
            browser: None,
        },
    );
    let before = world.agents[&agent()].clone();
    let revision = fold(
        &mut world,
        200,
        Record::AgentMcpServersChanged {
            agent: agent(),
            mcp_servers: names(&["b"]),
        },
    );
    let row = &world.agents[&agent()];
    assert_eq!(row.mcp.hire_servers, names(&["a"]));
    assert_eq!(row.mcp.hire_lsn, Lsn::from_parts(1, 100).unwrap());
    assert_eq!(row.mcp.desired, names(&["b"]));
    assert_eq!(row.mcp.revision, revision);
    assert_eq!(row.mcp.first_edit, Some(revision));
    assert!(row.paused);
    assert_eq!(row.control_lsn, before.control_lsn);
    assert_eq!(row.context_lsn, before.context_lsn);
    assert_eq!(row.session, before.session);
    assert_eq!(row.budget, before.budget);
    assert_eq!(row.goal, before.goal);
    assert!(world.mcp_grant(revision).is_none());
}

#[test]
fn desired_changes_do_not_rewrite_a_paired_grant_and_empty_clear_stays_active() {
    let mut world = hired();
    issue(&mut world);
    let policy = fold(
        &mut world,
        300,
        Record::AgentMcpServersChanged {
            agent: agent(),
            mcp_servers: names(&["b"]),
        },
    );
    let grant = fold(
        &mut world,
        400,
        Record::McpLaunchGranted {
            agent: agent(),
            turn: turn(),
            policy_lsn: policy,
            mcp_servers: names(&["b"]),
        },
    );
    assert!(world.mcp_grant(grant).unwrap().spawned.is_none());
    assert!(world.turns[&turn()].is_issued_not_spawned());
    fold(&mut world, 500, spawn(Some(grant), 42));
    let clear = fold(
        &mut world,
        600,
        Record::AgentMcpServersChanged {
            agent: agent(),
            mcp_servers: Vec::new(),
        },
    );
    assert!(world.agents[&agent()].mcp.desired.is_empty());
    assert_eq!(world.agents[&agent()].mcp.revision, clear);
    assert_eq!(world.agents[&agent()].mcp.first_edit, Some(policy));
    assert_eq!(world.turns[&turn()].mcp_grant, Some(grant));
    assert_eq!(world.mcp_grant(grant).unwrap().mcp_servers, names(&["b"]));
    assert_eq!(
        world
            .mcp_grant(grant)
            .unwrap()
            .spawned
            .as_ref()
            .unwrap()
            .1
            .pid,
        42
    );
}

#[test]
fn post_edit_receipts_and_grants_reject_missing_wrong_stale_and_reused_authority() {
    let mut world = hired();
    issue(&mut world);
    let policy = fold(
        &mut world,
        300,
        Record::AgentMcpServersChanged {
            agent: agent(),
            mcp_servers: names(&["b"]),
        },
    );
    let next = Lsn::from_parts(1, 500).unwrap();
    assert_eq!(
        world.validate_mcp_record(next, &spawn(None, 42)),
        Err(McpHistoryError::MissingGrant)
    );
    assert_eq!(
        world.validate_mcp_record(next, &spawn(Some(policy), 42)),
        Err(McpHistoryError::WrongGrant)
    );
    assert_eq!(
        world.validate_mcp_record(
            next,
            &Record::McpLaunchGranted {
                agent: agent(),
                turn: turn(),
                policy_lsn: world.agents[&agent()].mcp.hire_lsn,
                mcp_servers: names(&["a"]),
            }
        ),
        Err(McpHistoryError::InvalidPolicy)
    );
    let grant = fold(
        &mut world,
        400,
        Record::McpLaunchGranted {
            agent: agent(),
            turn: turn(),
            policy_lsn: policy,
            mcp_servers: names(&["b"]),
        },
    );
    fold(&mut world, 500, spawn(Some(grant), 42));
    assert_eq!(
        world.validate_mcp_record(Lsn::from_parts(1, 600).unwrap(), &spawn(Some(grant), 43)),
        Err(McpHistoryError::ReusedGrant)
    );
}

#[test]
fn a_never_edited_hire_has_no_grant_and_equal_or_noncanonical_records_are_invalid() {
    let mut world = hired();
    issue(&mut world);
    fold(&mut world, 300, spawn(None, 42));
    assert_eq!(world.agents[&agent()].mcp.first_edit, None);
    assert_eq!(world.turns[&turn()].mcp_grant, None);
    let next = Lsn::from_parts(1, 400).unwrap();
    for (values, expected) in [
        (&["a"][..], McpHistoryError::UnchangedSet),
        (&["a", "a"][..], McpHistoryError::InvalidSet),
        (&["b", "a"][..], McpHistoryError::InvalidSet),
    ] {
        assert_eq!(
            world.validate_mcp_record(
                next,
                &Record::AgentMcpServersChanged {
                    agent: agent(),
                    mcp_servers: names(values)
                }
            ),
            Err(expected)
        );
    }
    assert_eq!(
        world.validate_mcp_record(
            next,
            &Record::McpLaunchGranted {
                agent: agent(),
                turn: turn(),
                policy_lsn: world.agents[&agent()].mcp.revision,
                mcp_servers: names(&["a"])
            }
        ),
        Err(McpHistoryError::InvalidPolicy)
    );
}
