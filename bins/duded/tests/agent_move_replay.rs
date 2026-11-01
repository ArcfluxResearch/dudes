//! A recorded move is checked against the history before it, so a forged one stops replay before
//! the daemon binds a socket, and the one an honest history holds replays to the same agent in
//! the daemon's World and in the saved projection.
#![cfg(unix)]

use std::sync::Arc;

use duded::boot;
use duded::startup::Witness;
use dudes_journal::{Projections, Reader, replay};
use dudes_proto::placement::AgentMove;
use dudes_proto::record::{RecordedWorkspace, samples};
use dudes_proto::{AgentBudget, Entry, Record, RecordKind};
use dudes_supervisor::world::MoveHistoryError;
use dudes_types::{AgentId, SessionId, Timestamp, TurnId};

mod common;
use common::{Fixture, ScriptedRunner, SharedHost, TestClock};

fn agent() -> AgentId {
    let Record::AgentHired { id, .. } = samples::record(RecordKind::AgentHired) else {
        unreachable!("the sample is a hire");
    };
    id
}

fn workspace(id: &str) -> RecordedWorkspace {
    RecordedWorkspace {
        id: id.into(),
        label: id.into(),
    }
}

fn hire(fixture: &Fixture, project: &str) -> Record {
    let mut hired = samples::record(RecordKind::AgentHired);
    let Record::AgentHired {
        workspace: slot,
        cwd,
        sandbox,
        isolation,
        max_turns,
        max_budget,
        trusted,
        mcp_servers,
        worktree,
        ..
    } = &mut hired
    else {
        unreachable!("the sample is a hire");
    };
    (*sandbox, *isolation, *max_turns, *max_budget, *trusted) = (None, None, None, None, true);
    mcp_servers.clear();
    *worktree = None;
    *cwd = fixture.state.path().to_path_buf();
    *slot = Some(workspace(project));
    hired
}

fn moved(project: &str, cwd: &std::path::Path) -> Record {
    Record::AgentMoved {
        agent: agent(),
        to: AgentMove {
            cwd: cwd.to_path_buf(),
            workspace: workspace(project),
            worktree: None,
            budget: AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
        },
    }
}

fn learned(session: &str) -> Record {
    Record::SessionLearned {
        agent: agent(),
        turn: TurnId::from_parts(9, 9),
        session: SessionId::new(session),
    }
}

fn seed(fixture: &Fixture, records: Vec<Record>) -> duded::Settings {
    let settings = fixture.settings();
    let (mut writer, _) = dudes_journal::open(&fixture.journal_dir(), settings.journal).unwrap();
    let mut catalog = None;
    for mut record in records {
        if let Record::TicketCreatedWithRepositories(ticket) = &mut record {
            ticket.declaration.catalog_lsn = catalog.unwrap();
            ticket.declaration.project = "service".into();
        }
        let is_catalog = matches!(record, Record::ProjectRepositoriesRecorded { .. });
        let at = writer
            .append(&Entry {
                at: Timestamp::from_unix_millis(1_000),
                record,
            })
            .unwrap();
        if is_catalog {
            catalog = Some(at);
        }
    }
    settings
}

fn seams(
    settings: &duded::Settings,
) -> duded::settings::Seams<SharedHost, ScriptedRunner, TestClock, common::FixtureRng> {
    common::seams(
        settings,
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    )
}

fn boot_error(fixture: &Fixture, settings: &duded::Settings) -> String {
    let before = fixture.records();
    let result = boot(settings, seams(settings));
    assert_eq!(fixture.records(), before, "refused replay appends nothing");
    match result {
        Ok(_) => panic!("a forged move was admitted"),
        Err(error) => format!("{error:#}"),
    }
}

fn billing(fixture: &Fixture) -> std::path::PathBuf {
    let path = fixture.runtime.path().join("billing");
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn owned_by_agent(kind: RecordKind) -> Record {
    let mut record = samples::record(kind);
    match &mut record {
        Record::TurnIssued { agent: owner, .. } | Record::GoalSet { agent: owner, .. } => {
            *owner = agent();
        }
        _ => unreachable!("only issue and goal samples are owned"),
    }
    record
}

fn in_review() -> Vec<Record> {
    let Record::Claim { card, .. } = samples::record(RecordKind::Claim) else {
        unreachable!()
    };
    vec![
        samples::record(RecordKind::TicketCreatedWithRepositories),
        samples::record(RecordKind::Claim),
        Record::CardTransition {
            card,
            from: dudes_types::CardStatus::Claimed,
            to: dudes_types::CardStatus::Review,
            by: dudes_proto::Actor::Agent { agent: agent() },
            expected_seq: dudes_types::Seq(3),
            reason: None,
        },
    ]
}

fn answered() -> Vec<Record> {
    vec![
        Record::HumanQuestion {
            agent: agent(),
            text: "which branch?".into(),
        },
        Record::HumanAnswer {
            agent: agent(),
            text: "main".into(),
            provenance: None,
            browser_answer: None,
        },
    ]
}

fn abandoned_copy(fixture: &Fixture, elsewhere: &std::path::Path) -> Vec<Record> {
    let copy = dudes_types::WorktreeId::from_bytes([7; 16]);
    let Record::WorktreeLeased {
        origin,
        branch,
        base,
        ..
    } = samples::record(RecordKind::WorktreeLeased)
    else {
        unreachable!()
    };
    let mut away = hire(fixture, "billing");
    let Record::AgentHired { worktree, cwd, .. } = &mut away else {
        unreachable!()
    };
    (*worktree, *cwd) = (Some(copy), elsewhere.to_path_buf());
    vec![
        Record::WorktreeLeased {
            worktree: copy,
            origin,
            path: elsewhere.to_path_buf(),
            branch,
            base,
            from: None,
        },
        away,
        moved("service", fixture.state.path()),
        moved("billing", elsewhere),
    ]
}

#[test]
fn a_forged_move_stops_replay_before_the_socket_binds() {
    let fixture = Fixture::new();
    let elsewhere = billing(&fixture);
    let busy = |extra: Vec<Record>| {
        let mut records = vec![hire(&fixture, "service")];
        records.extend(extra);
        records.push(moved("billing", &elsewhere));
        records
    };
    let (issued, goal_set) = (
        owned_by_agent(RecordKind::TurnIssued),
        owned_by_agent(RecordKind::GoalSet),
    );
    let retired = Record::AgentRetired {
        id: agent(),
        reason: "done".into(),
        browser_retire: None,
    };
    let in_review = in_review();
    let answered = answered();
    let abandoned = abandoned_copy(&fixture, &elsewhere);
    let stories: [(&str, MoveHistoryError, Vec<Record>); 10] = [
        (
            "a copy it left behind",
            MoveHistoryError::AbandonedCopy,
            abandoned,
        ),
        ("a ticket in review", MoveHistoryError::Unfinished, {
            let mut records = vec![
                samples::record(RecordKind::ProjectRepositoriesRecorded),
                hire(&fixture, "service"),
            ];
            records.extend(in_review);
            records.push(moved("billing", &elsewhere));
            records
        }),
        (
            "an attached agent",
            MoveHistoryError::Busy,
            busy(vec![samples::record(RecordKind::Attached)]),
        ),
        (
            "an answer not yet delivered",
            MoveHistoryError::UndeliveredAnswers,
            busy(answered),
        ),
        (
            "an unknown agent",
            MoveHistoryError::UnknownAgent,
            vec![moved("billing", &elsewhere)],
        ),
        (
            "a retired agent",
            MoveHistoryError::UnknownAgent,
            busy(vec![retired]),
        ),
        ("an open turn", MoveHistoryError::Busy, busy(vec![issued])),
        (
            "an unfinished goal",
            MoveHistoryError::Unfinished,
            busy(vec![goal_set]),
        ),
        (
            "the project it is in",
            MoveHistoryError::SameProject,
            vec![hire(&fixture, "service"), moved("service", &elsewhere)],
        ),
        (
            "an own copy it lacks",
            MoveHistoryError::ForeignCopy,
            vec![hire(&fixture, "service"), {
                let mut record = moved("billing", &elsewhere);
                let Record::AgentMoved { to, .. } = &mut record else {
                    unreachable!()
                };
                to.worktree = Some(dudes_types::WorktreeId::from_bytes([7; 16]));
                record
            }],
        ),
    ];
    for (name, reason, records) in stories {
        let fixture = Fixture::new();
        let settings = seed(&fixture, records);
        let error = boot_error(&fixture, &settings);
        assert!(error.contains("invalid agent move"), "{name}: {error}");
        assert!(error.contains(&reason.to_string()), "{name}: {error}");
        assert!(
            std::os::unix::net::UnixStream::connect(settings.socket()).is_err(),
            "{name}"
        );
    }
}

#[test]
fn an_honest_history_replays_to_the_same_agent_in_the_world_and_the_saved_projection() {
    let fixture = Fixture::new();
    let elsewhere = billing(&fixture);
    let home = fixture.state.path().to_path_buf();
    let records = vec![
        hire(&fixture, "service"),
        learned("service-session"),
        moved("billing", &elsewhere),
        learned("billing-session"),
        moved("service", &home),
    ];
    let settings = seed(&fixture, records);
    let daemon = boot(&settings, seams(&settings)).expect("an honest history replays");
    let world_row = daemon.world().agents[&agent()].clone();
    let here = world_row.repositories.singleton().unwrap();
    assert_eq!(here.cwd(), home);
    assert_eq!(here.workspace().unwrap().id, "service");
    assert!(world_row.trusted);
    assert_eq!(world_row.session, Some(SessionId::new("service-session")));
    assert_eq!(
        world_row.other_projects["billing"].session,
        Some(SessionId::new("billing-session"))
    );
    let moves = Reader::open(&fixture.journal_dir())
        .unwrap()
        .0
        .iter_from(dudes_types::Lsn::ZERO)
        .unwrap()
        .map(Result::unwrap)
        .filter(|(_, entry)| matches!(entry.record, Record::AgentMoved { .. }))
        .map(|(lsn, _)| lsn)
        .collect::<Vec<_>>();
    assert_eq!(
        world_row.mcp.hire_lsn, moves[1],
        "the hire position is the latest move"
    );

    let mut running = common::start(daemon);
    running.stop(duded::ExitCause::Ended);
    let cache = settings.state_dir.join(duded::boot::PROJECTION_FILE);
    let reader = Reader::open(&fixture.journal_dir()).unwrap().0;
    let saved = Projections::open(&cache).unwrap().agents().unwrap();
    let mut rebuilt = Projections::open(&fixture.runtime.path().join("rebuilt.redb")).unwrap();
    replay(&reader, &mut rebuilt).unwrap();
    assert_eq!(rebuilt.agents().unwrap(), saved);
    let projected = &saved[0];
    assert_eq!(projected.session, world_row.session);
    assert_eq!(projected.session_lsn, world_row.session_lsn);
    assert_eq!(projected.hired_lsn, world_row.mcp.hire_lsn);
    assert_eq!(projected.trusted, world_row.trusted);
    assert_eq!(projected.other_projects, world_row.other_projects);
    assert_eq!(projected.repositories, world_row.repositories);
}
