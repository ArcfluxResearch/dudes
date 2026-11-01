//! An idle agent of one project takes a ticket in another: the daemon moves it as the ticket
//! starts, the move is a record of its own that a restart accepts, and the ticket then starts as
//! any other. These run over the same stand-in engine as the rest of the ticket flow.

use super::*;

const NOTE: &str = "Your earlier projects are not in this conversation";

fn docs_repo(fixture: &Fixture) -> PathBuf {
    let docs = fixture.state.path().join("docs");
    std::fs::create_dir_all(docs.join(".git")).unwrap();
    docs.canonicalize().unwrap()
}

/// Settings that enroll `service` and `docs`, each with a check that passes.
fn two_projects(fixture: &Fixture) -> (duded::Settings, PathBuf, PathBuf) {
    let (mut settings, repo) = enrolled(fixture);
    let docs = docs_repo(fixture);
    settings.enrollment.profiles.push(profile(
        "docs",
        &docs,
        vec![check("default", common::true_program())],
    ));
    (settings, repo, docs)
}

fn cody() -> AgentId {
    sample_agent()
}

fn dana() -> AgentId {
    AgentId::from_parts(9, 2)
}

fn hired(
    id: AgentId,
    name: &str,
    project: &str,
    repo: &Path,
    budget: Option<AgentBudget>,
) -> Record {
    let mut record = seeded_hire(project, repo, budget);
    let Record::AgentHired {
        id: slot,
        name: label,
        ..
    } = &mut record
    else {
        unreachable!("the sample is a hire");
    };
    (*slot, *label) = (id, name.to_owned());
    record
}

fn card(n: u128) -> CardId {
    CardId::from_parts(4, n)
}

fn ticket_for(n: u128, project: &str, agent: AgentId) -> Vec<Record> {
    vec![
        Record::TicketCreated {
            card: card(n),
            title: format!("Ticket {n}"),
            description: String::new(),
            department: "engineering".into(),
            project: project.into(),
            done_when: default_check(),
            by: operator(),
            request: None,
            priority: None,
        },
        Record::TicketAssigned {
            card: card(n),
            agent,
            by: operator(),
            message: None,
            expected_seq: Seq(0),
            priority: None,
        },
    ]
}

fn learned(agent: AgentId, session: &str) -> Record {
    Record::SessionLearned {
        agent,
        turn: TurnId::from_parts(9, 9),
        session: dudes_types::SessionId::new(session),
    }
}

fn moved_agents(fixture: &Fixture) -> Vec<(AgentId, dudes_proto::placement::AgentMove)> {
    fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::AgentMoved { agent, to } => Some((agent, to)),
            _ => None,
        })
        .collect()
}

/// The saved agents projection agrees with one rebuilt from the journal, after a daemon that
/// moved agents has stopped.
fn assert_projection_replays(fixture: &Fixture) {
    let settings = fixture.settings();
    let reader = dudes_journal::Reader::open(&fixture.journal_dir())
        .unwrap()
        .0;
    let saved =
        dudes_journal::Projections::open(&settings.state_dir.join(duded::boot::PROJECTION_FILE))
            .unwrap()
            .agents()
            .unwrap();
    let mut rebuilt =
        dudes_journal::Projections::open(&fixture.runtime.path().join("rebuilt.redb")).unwrap();
    dudes_journal::replay(&reader, &mut rebuilt).unwrap();
    assert_eq!(rebuilt.agents().unwrap(), saved);
}

/// Restart on the same directories: boot refuses any journal whose moves the history forbids.
fn restart_accepts(fixture: &Fixture, settings: &duded::Settings, repo: &Path) {
    let flow = Flow::boot(
        fixture,
        settings.clone(),
        repo.to_path_buf(),
        [common::fails()],
    );
    flow.stop();
}

fn spawn_cwds(flow: &Flow) -> Vec<PathBuf> {
    flow.host
        .spawned()
        .into_iter()
        .map(|spec| spec.cwd)
        .collect()
}

fn resumes(spec: &LaunchSpec, session: &str) -> bool {
    spec.argv
        .windows(2)
        .any(|pair| pair[0] == "--resume" && pair[1] == session)
}

#[test]
fn an_idle_agent_takes_a_ticket_in_another_project_and_runs_there() {
    let fixture = Fixture::new();
    let (settings, repo, docs) = two_projects(&fixture);
    let mut records = vec![hired(cody(), "Cody", "service", &repo, None)];
    records.extend(ticket_for(1, "docs", cody()));
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings.clone(), repo.clone(), [common::fails()]);
    flow.await_status(card(1), CardStatus::Doing);
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    fixture.await_record("the moved agent's spawn", |record| {
        matches!(record, Record::TurnSpawned { turn: owned, .. } if *owned == turn).then_some(())
    });

    let kinds = kinds(&fixture);
    let moved = kinds
        .iter()
        .position(|kind| *kind == RecordKind::AgentMoved)
        .unwrap();
    let started = kinds
        .iter()
        .position(|kind| *kind == RecordKind::TicketStarted)
        .unwrap();
    assert!(moved < started, "{kinds:?}");
    let moves = moved_agents(&fixture);
    assert_eq!(moves.len(), 1);
    assert_eq!(
        (moves[0].0, moves[0].1.workspace.id.as_str()),
        (cody(), "docs")
    );
    assert_eq!(moves[0].1.cwd, docs);
    assert_eq!(spawn_cwds(&flow), [docs.clone()]);
    let spec = &flow.host.spawned()[0];
    assert!(
        !spec.argv.iter().any(|part| part == "--resume"),
        "a fresh session"
    );
    let (_, _, _, task) = &Flow::turns(&fixture)[0];
    assert!(task.contains(NOTE), "{task}");
    flow.stop();

    assert_projection_replays(&fixture);
    restart_accepts(&fixture, &settings, &repo);
    assert_eq!(
        moved_agents(&fixture).len(),
        1,
        "a restart moves nothing again"
    );
}

#[test]
fn an_agent_returns_to_its_home_conversation_and_the_projects_ceiling_binds_only_there() {
    let fixture = Fixture::new();
    let (mut settings, repo, docs) = two_projects(&fixture);
    settings
        .enrollment
        .profiles
        .iter_mut()
        .find(|profile| profile.id == "docs")
        .unwrap()
        .agent_budget = AgentBudget {
        usd_per_day: Some(Usd::from_dollars(2)),
        tokens_per_day: None,
    };
    let own = AgentBudget {
        usd_per_day: Some(Usd::from_dollars(5)),
        tokens_per_day: None,
    };
    let mut records = vec![
        hired(cody(), "Cody", "service", &repo, Some(own)),
        learned(cody(), "service-session"),
    ];
    records.extend(ticket_for(1, "docs", cody()));
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings.clone(), repo.clone(), [common::passes()]);
    flow.await_status(card(1), CardStatus::Doing);
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(&fixture, turn, Some("Done."));
    flow.await_completed_review(card(1));
    assert!(flow.accept(card(1)).error.is_none());

    let back = flow.create_with(json!({"title": "Back home", "project": "service"}));
    flow.give_ok(back, cody(), None);
    flow.await_status(back, CardStatus::Doing);
    let (_, _, _, task) = Flow::await_turn_number(&fixture, 2);
    assert!(
        !task.contains(NOTE),
        "a conversation that resumes needs no note: {task}"
    );
    fixture.await_record("the second spawn", |record| {
        matches!(record, Record::TurnSpawned { turn: second, .. } if *second != turn).then_some(())
    });
    let spawned = flow.host.spawned();
    assert_eq!(spawned.len(), 2);
    assert_eq!(spawned[1].cwd, repo);
    assert!(
        resumes(&spawned[1], "service-session"),
        "{:?}",
        spawned[1].argv
    );
    assert!(!resumes(&spawned[0], "service-session"));

    let moves = moved_agents(&fixture);
    assert_eq!(moves.len(), 2);
    assert_eq!(moves[0].1.cwd, docs);
    assert_eq!(moves[0].1.budget.usd_per_day, Some(Usd::from_dollars(2)));
    assert_eq!(
        moves[1].1.budget.usd_per_day,
        Some(Usd::from_dollars(5)),
        "the project's ceiling lifts when the agent leaves"
    );
    flow.stop();
    assert_projection_replays(&fixture);
    restart_accepts(&fixture, &settings, &repo);
}

#[test]
fn a_busy_agent_waits_and_a_ticket_ready_for_you_holds_the_move_until_it_is_reviewed() {
    let fixture = Fixture::new();
    let (settings, repo, _) = two_projects(&fixture);
    // The service ticket was given first, so queue order serves it first although `docs` sorts
    // before `service`.
    let mut records = vec![hired(cody(), "Cody", "service", &repo, None)];
    records.extend(ticket_for(1, "service", cody()));
    records.extend(ticket_for(2, "docs", cody()));
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings.clone(), repo.clone(), [common::passes()]);
    flow.await_status(card(1), CardStatus::Doing);
    assert_eq!(flow.card(card(2)).status, CardStatus::Claimed);
    assert!(
        moved_agents(&fixture).is_empty(),
        "a busy agent does not move"
    );
    assert_eq!(
        flow.read(card(2)).waiting.unwrap().wait,
        TicketWait::AgentBusy
    );

    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(&fixture, turn, Some("Done."));
    flow.await_completed_review(card(1));
    assert_eq!(flow.card(card(2)).status, CardStatus::Claimed);
    assert!(
        moved_agents(&fixture).is_empty(),
        "Ready for you holds the agent"
    );
    assert_eq!(
        flow.read(card(2)).waiting.unwrap().wait,
        TicketWait::Ticket { card: card(1) }
    );

    assert!(flow.accept(card(1)).error.is_none());
    flow.await_status(card(2), CardStatus::Doing);
    assert_eq!(moved_agents(&fixture).len(), 1);
    flow.stop();
    assert_projection_replays(&fixture);
}

#[test]
fn an_arriving_agent_waits_behind_the_ticket_in_the_folder_it_will_use() {
    let fixture = Fixture::new();
    let (settings, repo, docs) = two_projects(&fixture);
    let mut records = vec![
        hired(cody(), "Cody", "service", &repo, None),
        hired(dana(), "Dana", "docs", &docs, None),
    ];
    records.extend(ticket_for(1, "docs", dana()));
    records.extend(ticket_for(2, "docs", cody()));
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings.clone(), repo.clone(), [common::passes()]);
    flow.await_status(card(1), CardStatus::Doing);
    assert_eq!(flow.card(card(2)).status, CardStatus::Claimed);
    assert!(
        moved_agents(&fixture).is_empty(),
        "the arriving agent is not cleared into a held folder"
    );
    assert_eq!(
        flow.read(card(2)).waiting.unwrap().wait,
        TicketWait::Ticket { card: card(1) }
    );

    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(&fixture, turn, Some("Done."));
    flow.await_completed_review(card(1));
    assert!(moved_agents(&fixture).is_empty());
    assert!(flow.accept(card(1)).error.is_none());
    flow.await_status(card(2), CardStatus::Doing);
    assert_eq!(moved_agents(&fixture).len(), 1);
    flow.stop();
}

#[test]
fn a_check_program_is_judged_in_the_folder_the_agent_will_work_in() {
    let fixture = Fixture::new();
    let (mut settings, repo, docs) = two_projects(&fixture);
    let script = docs.join("check.sh");
    std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let program = script.to_string_lossy().into_owned();
    settings
        .enrollment
        .profiles
        .iter_mut()
        .find(|profile| profile.id == "docs")
        .unwrap()
        .checks
        .push(check("inside", &program));
    let mut ticket = ticket_for(1, "docs", cody());
    let Record::TicketCreated { done_when, .. } = &mut ticket[0] else {
        unreachable!("the first record creates the ticket");
    };
    *done_when = TicketCheck {
        id: "inside".into(),
        label: "inside passes".into(),
        predicate: Predicate::Command {
            argv: vec![program],
            timeout_s: 10,
        },
    };
    let mut records = vec![hired(cody(), "Cody", "service", &repo, None)];
    records.extend(ticket);
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    assert_needs_you(
        &mut flow,
        card(1),
        TicketReason::CheckCannotRun,
        TicketReason::CheckCannotRun.sentence(),
    );
    assert!(
        moved_agents(&fixture).is_empty(),
        "the move waits for the check to be runnable"
    );
    flow.stop();
}

#[test]
fn a_recorded_inherit_or_bare_isolation_does_not_move_and_a_plain_hire_does() {
    for isolation in [IsolationMode::Inherit, IsolationMode::Bare] {
        let fixture = Fixture::new();
        let (settings, repo, _) = two_projects(&fixture);
        let mut hire = hired(cody(), "Cody", "service", &repo, None);
        let Record::AgentHired {
            isolation: slot, ..
        } = &mut hire
        else {
            unreachable!("the sample is a hire");
        };
        *slot = Some(isolation);
        seed(&fixture, vec![hire]);
        let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
        let card = flow.create_with(json!({"title": "Docs work", "project": "docs"}));
        let response = flow.give(card, cody(), None);
        let refusal = response.error.expect("the give is refused");
        assert!(
            refusal.message.contains("isolation"),
            "{isolation:?}: {}",
            refusal.message
        );
        assert_eq!(flow.card(card).status, CardStatus::Todo);
        assert!(moved_agents(&fixture).is_empty());
        flow.stop();
    }

    let fixture = Fixture::new();
    let (settings, repo, _) = two_projects(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    let agent = flow.hire("Cody");
    let card = flow.create_with(json!({"title": "Docs work", "project": "docs"}));
    assert_eq!(
        flow.give_ok(card, agent, None),
        TicketAssignOutcome::Started
    );
    assert_eq!(moved_agents(&fixture).len(), 1);
    flow.stop();
}

#[test]
fn trust_in_one_project_does_not_reach_another() {
    let fixture = Fixture::new();
    let (settings, repo, _) = two_projects(&fixture);
    let mut hire = hired(cody(), "Cody", "service", &repo, None);
    let Record::AgentHired { trusted, .. } = &mut hire else {
        unreachable!("the sample is a hire");
    };
    *trusted = true;
    let mut records = vec![hire];
    records.extend(ticket_for(1, "docs", cody()));
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    flow.await_status(card(1), CardStatus::Doing);
    flow.stop();
    let cache = fixture
        .settings()
        .state_dir
        .join(duded::boot::PROJECTION_FILE);
    let row = &dudes_journal::Projections::open(&cache)
        .unwrap()
        .agents()
        .unwrap()[0];
    assert!(!row.trusted, "the new place is untrusted");
    assert!(
        row.other_projects["service"].trusted,
        "the old place keeps its grant"
    );
}

#[test]
fn an_agent_with_a_ticket_in_each_project_starts_them_in_queue_order() {
    let fixture = Fixture::new();
    let (settings, repo, _) = two_projects(&fixture);
    let mut records = vec![hired(cody(), "Cody", "service", &repo, None)];
    records.extend(ticket_for(1, "docs", cody()));
    records.extend(ticket_for(2, "service", cody()));
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    flow.await_status(card(1), CardStatus::Doing);
    assert_eq!(flow.card(card(2)).status, CardStatus::Claimed);
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(&fixture, turn, Some("Done."));
    flow.await_completed_review(card(1));
    assert!(flow.accept(card(1)).error.is_none());
    flow.await_status(card(2), CardStatus::Doing);
    let kinds: Vec<_> = moved_agents(&fixture)
        .iter()
        .map(|(_, to)| to.workspace.id.clone())
        .collect();
    assert_eq!(kinds, ["docs", "service"]);
    flow.stop();
    assert_projection_replays(&fixture);
}

#[test]
fn a_crash_between_the_move_and_the_start_ends_in_one_started_ticket() {
    let fixture = Fixture::new();
    let (settings, repo, _) = two_projects(&fixture);
    let mut records = vec![hired(cody(), "Cody", "service", &repo, None)];
    records.extend(ticket_for(1, "docs", cody()));
    seed(&fixture, records);
    let flow = Flow::boot_with(
        &fixture,
        settings.clone(),
        repo.clone(),
        [common::fails()],
        Options {
            refuse_nth_append: std::num::NonZeroUsize::new(2),
            ..Options::default()
        },
    );
    fixture.await_record("the move", |record| {
        matches!(record, Record::AgentMoved { .. }).then_some(())
    });
    flow.stop();
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    flow.await_status(card(1), CardStatus::Doing);
    flow.stop();
    assert_eq!(moved_agents(&fixture).len(), 1);
    assert_eq!(started_goals(&fixture.records()).len(), 1);
}

#[test]
fn the_folder_of_a_ticket_whose_agent_has_moved_is_not_opened() {
    let fixture = Fixture::new();
    let (settings, repo, _) = two_projects(&fixture);
    let mut records = vec![hired(cody(), "Cody", "service", &repo, None)];
    records.extend(ticket_for(1, "service", cody()));
    records.extend(ticket_for(2, "docs", cody()));
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    flow.await_status(card(1), CardStatus::Doing);
    let open = |flow: &mut Flow, card| {
        let detail = flow.card(card);
        let cred = credential(&flow.settings);
        flow.operator
            .call_current_as_operator(
                dudes_proto::rpc::CurrentMethod::TicketCheckout,
                Some(json!({"card": card, "expected_seq": detail.seq})),
                cred,
            )
            .unwrap()
    };
    assert!(open(&mut flow, card(1)).error.is_none());
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(&fixture, turn, Some("Done."));
    flow.await_completed_review(card(1));
    assert!(flow.accept(card(1)).error.is_none());
    flow.await_status(card(2), CardStatus::Doing);
    let refused = open(&mut flow, card(1))
        .error
        .expect("the folder is no longer this ticket's");
    assert!(
        refused.message.contains("moved to another project"),
        "{}",
        refused.message
    );
    flow.stop();
}

#[test]
fn an_agent_hired_without_a_project_is_refused_with_its_reason() {
    let fixture = Fixture::new();
    let (settings, repo, _) = two_projects(&fixture);
    let mut hire = hired(cody(), "Cody", "service", &repo, None);
    let Record::AgentHired { workspace, .. } = &mut hire else {
        unreachable!("the sample is a hire");
    };
    *workspace = None;
    seed(&fixture, vec![hire]);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    let card = flow.create_with(json!({"title": "Docs work", "project": "docs"}));
    let response = flow.give(card, cody(), None);
    assert_eq!(
        refusal_reason(&response),
        TicketRefusal::AgentNoProject.code()
    );
    flow.stop();
}

#[test]
fn an_agent_in_its_own_copy_returns_to_that_copy_and_its_conversation() {
    let fixture = Fixture::new();
    let (settings, repo, docs) = two_projects(&fixture);
    let copy = fixture.state.path().join("copy-3");
    let agent = AgentId::from_parts(9, 3);
    let mut records = copy_hire(3, &repo, Some(&copy));
    records.push(learned(agent, "copy-session"));
    records.extend(ticket_for(1, "docs", agent));
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings.clone(), repo.clone(), [common::passes()]);
    flow.await_status(card(1), CardStatus::Doing);
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(&fixture, turn, Some("Done."));
    flow.await_completed_review(card(1));
    assert!(flow.accept(card(1)).error.is_none());
    let back = flow.create_with(json!({"title": "Back home", "project": "service"}));
    flow.give_ok(back, agent, None);
    flow.await_status(back, CardStatus::Doing);
    fixture.await_record("the return spawn", |record| {
        matches!(record, Record::TurnSpawned { turn: second, .. } if *second != turn).then_some(())
    });

    let moves = moved_agents(&fixture);
    assert_eq!(moves.len(), 2);
    assert_eq!((moves[0].1.cwd.clone(), moves[0].1.worktree), (docs, None));
    assert_eq!(moves[1].1.cwd, copy);
    assert!(
        moves[1].1.worktree.is_some(),
        "it returns to the copy it left"
    );
    let spawned = flow.host.spawned();
    assert_eq!(spawned[1].cwd, copy);
    assert!(
        resumes(&spawned[1], "copy-session"),
        "{:?}",
        spawned[1].argv
    );
    flow.stop();
    assert_projection_replays(&fixture);
    restart_accepts(&fixture, &settings, &repo);
}

#[test]
fn a_crash_in_the_first_turn_after_a_move_needs_the_operators_consent() {
    let fixture = Fixture::new();
    let (settings, repo, _) = two_projects(&fixture);
    let mut records = vec![hired(cody(), "Cody", "service", &repo, None)];
    records.extend(ticket_for(1, "service", cody()));
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings.clone(), repo.clone(), [common::passes()]);
    flow.await_status(card(1), CardStatus::Doing);
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(&fixture, turn, Some("Done."));
    flow.await_completed_review(card(1));
    assert!(flow.accept(card(1)).error.is_none());
    flow.stop();

    let gate = Arc::new(Gate::default());
    let mut flow = Flow::boot_with(
        &fixture,
        settings.clone(),
        repo.clone(),
        [common::fails()],
        Options {
            gate: Some(Arc::clone(&gate)),
            ..Options::default()
        },
    );
    let docs_card = flow.create_with(json!({"title": "Docs work", "project": "docs"}));
    let socket = flow.settings.clone();
    let giver = std::thread::spawn(move || {
        let mut client = Client::connect(socket.socket()).unwrap();
        client
            .call_as_operator(
                dudes_proto::rpc::Method::TicketAssign,
                Some(json!({"card": docs_card, "agent": cody(), "expected_seq": 0})),
                credential(&socket),
            )
            .unwrap()
    });
    gate.wait_reached();
    assert_eq!(moved_agents(&fixture).len(), 1);

    let second = Fixture::new();
    copy_dir(&fixture.journal_dir(), &second.journal_dir());
    let mut settings = second.settings();
    settings.enrollment = flow.settings.enrollment.clone();
    let mut revived = Flow::boot(&second, settings, repo, [common::fails()]);
    assert_eq!(
        revived.await_status(docs_card, CardStatus::Blocked).status,
        CardStatus::Blocked
    );
    assert!(revived.host.spawned().is_empty(), "no restart host call");
    assert_eq!(
        moved_agents(&second).len(),
        1,
        "the restart moves nothing again"
    );
    revived.stop();
    gate.release();
    let _ = giver.join();
    flow.stop();
}
