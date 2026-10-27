//! A ticket's start carries a whole goal, so replay checks it against the ticket instead of
//! trusting it. A forged start stops the daemon before it binds a socket, and the one the daemon
//! itself writes replays to a ticket that is working.
#![cfg(unix)]

use std::sync::Arc;

use duded::boot;
use duded::startup::Witness;
use dudes_proto::record::{RecordedWorkspace, samples};
use dudes_proto::{
    Actor, CheckBaseline, Entry, Goal, GoalBudget, Predicate, Record, RecordKind, TicketCheck,
};
use dudes_rpc::Client;
use dudes_types::{AgentId, CardId, CardStatus, GoalId, Seq, Timestamp};
use serde_json::json;

mod common;
use common::{Fixture, ScriptedRunner, SharedHost, TestClock};

fn agent() -> AgentId {
    let Record::AgentHired { id, .. } = samples::record(RecordKind::AgentHired) else {
        unreachable!("the sample is a hire");
    };
    id
}

fn card() -> CardId {
    CardId::from_parts(4, 1)
}

fn check() -> TicketCheck {
    TicketCheck {
        id: "default".into(),
        label: "tests pass".into(),
        predicate: Predicate::Command {
            argv: vec![common::true_program().into()],
            timeout_s: 30,
        },
    }
}

fn hire(project: &str) -> Record {
    let mut hired = samples::record(RecordKind::AgentHired);
    let Record::AgentHired {
        workspace,
        sandbox,
        isolation,
        max_turns,
        max_budget,
        trusted,
        mcp_servers,
        ..
    } = &mut hired
    else {
        unreachable!("the sample is a hire");
    };
    (*sandbox, *isolation, *max_turns, *max_budget, *trusted) = (None, None, None, None, false);
    mcp_servers.clear();
    *workspace = Some(RecordedWorkspace {
        id: project.into(),
        label: "Service".into(),
    });
    hired
}

fn honest_goal() -> Goal {
    let check = check();
    Goal {
        id: GoalId::from_parts(8, 1),
        text: "Fix the parser".into(),
        completion_check: duded::ticket_goal_check(&check),
        predicate: check.predicate,
        baseline: Some(CheckBaseline::MayAlreadyPass),
        budget: GoalBudget {
            usd: None,
            tokens: None,
        },
        hop_cap: 4,
    }
}

/// How a test spoils an honest goal.
type Forgery = fn(&mut Goal);

fn story(goal: Goal) -> Vec<Record> {
    let operator = Actor::Peer { pid: 1, uid: None };
    vec![
        hire("service"),
        Record::TicketCreated {
            card: card(),
            title: "Fix the parser".into(),
            description: String::new(),
            department: "engineering".into(),
            project: "service".into(),
            done_when: check(),
            by: operator,
            request: None,
        },
        Record::TicketAssigned {
            card: card(),
            agent: agent(),
            by: operator,
            message: None,
            expected_seq: Seq(0),
        },
        Record::TicketStarted {
            card: card(),
            agent: agent(),
            goal,
            expected_seq: Seq(1),
        },
    ]
}

fn seed(fixture: &Fixture, records: Vec<Record>) -> duded::Settings {
    let settings = fixture.settings();
    let (mut writer, _) = dudes_journal::open(&fixture.journal_dir(), settings.journal).unwrap();
    for record in records {
        writer
            .append(&Entry {
                at: Timestamp::from_unix_millis(1_000),
                record,
            })
            .unwrap();
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
        Ok(_) => panic!("a forged ticket start was admitted"),
        Err(error) => format!("{error:#}"),
    }
}

#[test]
fn a_forged_ticket_start_stops_replay_before_the_socket_binds() {
    let forgeries: [(&str, Forgery); 4] = [
        ("predicate", |goal| {
            goal.predicate = Predicate::Command {
                argv: vec![common::false_program().into()],
                timeout_s: 30,
            };
        }),
        ("baseline", |goal| goal.baseline = None),
        ("check name", |goal| {
            goal.completion_check.as_mut().unwrap().label = "Forged label".into();
        }),
        ("check name", |goal| goal.completion_check = None),
    ];
    for (needle, forge) in forgeries {
        let fixture = Fixture::new();
        let mut goal = honest_goal();
        forge(&mut goal);
        let settings = seed(&fixture, story(goal));
        let error = boot_error(&fixture, &settings);
        assert!(error.contains("ticket start"), "{error}");
        assert!(error.contains(needle), "{needle}: {error}");
        assert!(std::os::unix::net::UnixStream::connect(settings.socket()).is_err());
    }
}

#[test]
fn a_ticket_start_for_a_removed_or_foreign_ticket_stops_replay() {
    let fixture = Fixture::new();
    let mut records = story(honest_goal());
    records.insert(
        3,
        Record::TicketRemoved {
            card: card(),
            by: Actor::System,
            expected_seq: Seq(1),
        },
    );
    let settings = seed(&fixture, records);
    assert!(boot_error(&fixture, &settings).contains("absent or removed"));

    let fixture = Fixture::new();
    let mut records = story(honest_goal());
    records[0] = hire("another");
    let settings = seed(&fixture, records);
    assert!(boot_error(&fixture, &settings).contains("project"));

    let fixture = Fixture::new();
    let mut records = story(honest_goal());
    records.push(Record::TicketStarted {
        card: card(),
        agent: agent(),
        goal: Goal {
            id: GoalId::from_parts(8, 2),
            ..honest_goal()
        },
        expected_seq: Seq(2),
    });
    let settings = seed(&fixture, records);
    assert!(boot_error(&fixture, &settings).contains("queue"));
}

#[test]
fn a_start_that_reuses_a_goal_id_stops_replay() {
    let fixture = Fixture::new();
    let mut records = story(honest_goal());
    records.insert(
        1,
        Record::GoalSet {
            agent: agent(),
            goal: honest_goal(),
            browser_onboarding: None,
            browser_goal: None,
            goal_replacement: None,
        },
    );
    let settings = seed(&fixture, records);
    assert!(boot_error(&fixture, &settings).contains("goal id"));
}

#[test]
fn the_start_the_daemon_writes_replays_to_a_working_ticket() {
    let fixture = Fixture::new();
    let settings = seed(&fixture, story(honest_goal()));
    let daemon = boot(&settings, seams(&settings)).expect("an honest start replays");
    let mut running = common::start(daemon);
    let credential = dudes_proto::rpc::cred::OperatorCredential::new(
        std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .unwrap(),
    )
    .unwrap();
    let response = Client::connect(settings.socket())
        .unwrap()
        .call_as_operator(
            dudes_proto::rpc::Method::BoardList,
            Some(json!({})),
            credential,
        )
        .unwrap();
    let cards: Vec<dudes_proto::CardView> =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].id, card());
    assert_eq!(cards[0].status, CardStatus::Doing);
    assert_eq!(cards[0].claimed_by, Some(agent()));
    running.stop(duded::ExitCause::Ended);
}

#[test]
fn singleton_ticket_authority_survives_daemon_reopen_and_empty_cache_rebuild() {
    use dudes_journal::{Projections, Reader, replay};
    use dudes_types::Lsn;

    let fixture = Fixture::new();
    let mut records = story(honest_goal());
    let Record::AgentHired { cwd, .. } = &mut records[0] else {
        unreachable!();
    };
    *cwd = fixture.state.path().to_path_buf();
    records.push(Record::Pause {
        agent: agent(),
        browser: None,
    });
    let mut issue = samples::record(RecordKind::TurnIssued);
    let Record::TurnIssued {
        agent: owner, goal, ..
    } = &mut issue
    else {
        unreachable!();
    };
    *owner = agent();
    *goal = honest_goal().id;
    let turn = issue.turn_issue().unwrap().turn;
    records.push(issue);
    let settings = seed(&fixture, records);
    let host = SharedHost::new();
    let daemon = boot(
        &settings,
        common::seams(
            &settings,
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let authority = daemon.world().turns[&turn].repositories.clone().unwrap();
    let round = authority.ticket().unwrap();
    assert_eq!(
        (round.card, round.goal, round.round),
        (card(), honest_goal().id, 1)
    );
    assert_eq!(round.repositories.as_ref().unwrap().project(), "service");
    assert_eq!(authority.singleton().unwrap().cwd(), fixture.state.path());
    let mut running = common::start(daemon);
    running.stop(duded::ExitCause::Ended);
    assert!(
        host.spawned().is_empty(),
        "a paused issued turn remains unlaunched"
    );

    let cache = settings.state_dir.join(duded::boot::PROJECTION_FILE);
    let reader = Reader::open(&fixture.journal_dir()).unwrap().0;
    let live = Projections::open(&cache).unwrap();
    let expected_ticket = live.ticket(card()).unwrap().unwrap();
    let expected_export = live.export().unwrap();
    assert_eq!(expected_ticket.1.repositories.project(), "service");
    assert_eq!(expected_ticket.1.done_when, check());
    assert_eq!(expected_ticket.1.round, 1);
    assert_eq!(
        live.turn(turn).unwrap().unwrap().repositories.as_ref(),
        Some(&authority)
    );
    drop(live);
    let mut warm = Projections::open(&cache).unwrap();
    assert_eq!(replay(&reader, &mut warm).unwrap().applied, 0);
    assert_eq!(warm.ticket(card()).unwrap().unwrap(), expected_ticket);
    assert_eq!(warm.export().unwrap(), expected_export);
    drop(warm);

    let mut rebuilt = Projections::open(&fixture.runtime.path().join("rebuilt.redb")).unwrap();
    assert!(replay(&reader, &mut rebuilt).unwrap().applied > 0);
    assert_eq!(rebuilt.ticket(card()).unwrap().unwrap(), expected_ticket);
    assert_eq!(
        rebuilt.turn(turn).unwrap().unwrap().repositories.as_ref(),
        Some(&authority)
    );
    assert_eq!(rebuilt.export().unwrap(), expected_export);
    drop(rebuilt);
    std::fs::remove_file(cache).unwrap();
    let daemon = boot(&settings, seams(&settings)).unwrap();
    assert_eq!(
        daemon.world().turns[&turn].repositories.as_ref(),
        Some(&authority)
    );
    assert!(
        daemon
            .world()
            .last_applied_lsn
            .is_some_and(|lsn| lsn > Lsn::ZERO)
    );
    let mut reopened = common::start(daemon);
    reopened.stop(duded::ExitCause::Ended);
}
