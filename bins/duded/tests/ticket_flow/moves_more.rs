//! Moves that wait, are refused, or meet other work: what the operator sees when the agent cannot
//! go, and what the agent does once it has.

use super::*;

#[test]
fn only_the_first_task_after_a_move_carries_the_note() {
    let fixture = Fixture::new();
    let (settings, repo, _) = two_projects(&fixture);
    let mut records = vec![hired(cody(), "Cody", "service", &repo, None)];
    records.extend(ticket_for(1, "docs", cody()));
    records.extend(ticket_for(2, "docs", cody()));
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings.clone(), repo.clone(), [common::passes()]);
    flow.await_status(card(1), CardStatus::Doing);
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(&fixture, turn, Some("Done."));
    flow.await_completed_review(card(1));
    assert!(flow.accept(card(1)).error.is_none());
    flow.await_status(card(2), CardStatus::Doing);
    let (.., first) = Flow::await_turn_number(&fixture, 1);
    let (.., second) = Flow::await_turn_number(&fixture, 2);
    assert!(first.contains(NOTE), "{first}");
    assert!(
        !second.contains(NOTE),
        "the agent has been here since: {second}"
    );
    assert_eq!(moved_agents(&fixture).len(), 1);
    flow.stop();
    assert_projection_replays(&fixture);
    restart_accepts(&fixture, &settings, &repo);
}

fn several_repositories(settings: &mut duded::Settings, docs: &Path) {
    use dudes_proto::repository::{RepoId, RepositoryEntry, RepositorySet};
    let entries = ["web", "api"]
        .into_iter()
        .map(|name| {
            let path = docs.join(name);
            std::fs::create_dir_all(path.join(".git")).unwrap();
            let path = path.canonicalize().unwrap();
            RepositoryEntry {
                repo_id: RepoId::parse(name).unwrap(),
                value: duded::floor_actions::ConfiguredRepository {
                    label: name.to_owned(),
                    path: path.clone(),
                    written_path: path,
                },
            }
        })
        .collect();
    settings
        .enrollment
        .profiles
        .iter_mut()
        .find(|profile| profile.id == "docs")
        .unwrap()
        .repositories = Some(RepositorySet::new(RepoId::parse("web").unwrap(), entries).unwrap());
}

#[test]
fn a_project_with_several_repositories_is_refused_and_giving_again_says_why() {
    let fixture = Fixture::new();
    let (mut settings, repo, docs) = two_projects(&fixture);
    several_repositories(&mut settings, &docs);
    let mut records = vec![hired(cody(), "Cody", "service", &repo, None)];
    records.extend(ticket_for(1, "docs", cody()));
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    assert_needs_you(
        &mut flow,
        card(1),
        TicketReason::ProjectUnavailable,
        TicketReason::ProjectUnavailable.sentence(),
    );
    assert!(moved_agents(&fixture).is_empty());
    let response = flow.give(card(1), cody(), None);
    assert_eq!(
        refusal_reason(&response),
        TicketRefusal::AgentUnavailable.code()
    );
    let message = response.error.unwrap().message;
    assert!(
        message.contains("the project holds several repositories"),
        "{message}"
    );
    flow.stop();
}

#[test]
fn a_ticket_in_needs_you_holds_its_agent_in_its_project_until_it_is_stopped() {
    let fixture = Fixture::new();
    let (mut settings, repo, _) = two_projects(&fixture);
    hop_cap_one(&mut settings);
    let mut records = vec![hired(cody(), "Cody", "service", &repo, None)];
    records.extend(ticket_for(1, "service", cody()));
    records.extend(ticket_for(2, "docs", cody()));
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(&fixture, turn, Some("I tried."));
    flow.await_status(card(1), CardStatus::Blocked);
    assert_eq!(
        flow.read(card(1)).ticket.reason,
        Some(TicketReason::NoTurns)
    );
    assert_eq!(flow.card(card(2)).status, CardStatus::Claimed);
    assert!(
        moved_agents(&fixture).is_empty(),
        "Needs you holds the agent"
    );
    assert_eq!(
        flow.read(card(2)).waiting.unwrap().wait,
        TicketWait::AgentBusy
    );

    let params = stop_params(&mut flow, card(1));
    flow.ok::<Value>(dudes_proto::rpc::Method::TicketStop, params);
    flow.await_status(card(2), CardStatus::Doing);
    assert_eq!(moved_agents(&fixture).len(), 1);
    flow.stop();
    assert_projection_replays(&fixture);
}

#[test]
fn retiring_a_moved_agent_releases_the_copy_it_left_and_lists_it() {
    let fixture = Fixture::new();
    let (settings, repo, _) = two_projects(&fixture);
    let copy = fixture.state.path().join("copy-3");
    let agent = AgentId::from_parts(9, 3);
    let mut records = copy_hire(3, &repo, Some(&copy));
    records.extend(ticket_for(1, "docs", agent));
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings.clone(), repo.clone(), [common::passes()]);
    flow.await_status(card(1), CardStatus::Doing);
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(&fixture, turn, Some("Done."));
    flow.await_completed_review(card(1));
    assert!(flow.accept(card(1)).error.is_none());
    flow.ok::<Value>(
        dudes_proto::rpc::Method::AgentRetire,
        json!({"agent": agent, "reason": "done"}),
    );
    flow.stop();
    restart_accepts(&fixture, &settings, &repo);

    let cache = settings.state_dir.join(duded::boot::PROJECTION_FILE);
    let store = dudes_journal::Projections::open(&cache).unwrap();
    let listed = store.retired_agents().unwrap();
    let released = listed
        .iter()
        .find(|row| row.id == agent)
        .and_then(|row| row.worktree.as_ref())
        .expect("the copy is listed");
    assert_eq!(released.state, dudes_proto::WorktreeViewState::Released);
}

#[test]
fn a_chat_turn_with_no_goal_after_a_move_runs_in_the_new_folder() {
    let fixture = Fixture::new();
    let (mut settings, repo, docs) = two_projects(&fixture);
    floor_answers(&mut settings);
    let mut records = vec![hired(cody(), "Cody", "service", &repo, None)];
    records.extend(ticket_for(1, "docs", cody()));
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings.clone(), repo.clone(), [common::passes()]);
    flow.await_status(card(1), CardStatus::Doing);
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(&fixture, turn, Some("Done."));
    flow.await_completed_review(card(1));
    assert!(flow.accept(card(1)).error.is_none());

    let page = flow.page();
    let (head, receipt) = common::floor_post(
        &page,
        "/floor/chat/send",
        &json!({"request_id": request_id('K'), "agent": cody(), "text": "Where are you?"}),
    );
    assert!(head.starts_with("HTTP/1.1 200"), "{head} {receipt}");
    fixture.await_record("the chat turn's spawn", |record| {
        matches!(record, Record::TurnSpawned { turn: chat, .. } if *chat != turn).then_some(())
    });
    assert_eq!(spawn_cwds(&flow), [docs.clone(), docs]);
    assert_eq!(moved_agents(&fixture).len(), 1);
    flow.stop();
    assert_projection_replays(&fixture);
    restart_accepts(&fixture, &settings, &repo);
}

fn recovery_call(
    settings: &duded::Settings,
    method: dudes_proto::rpc::CurrentMethod,
    params: Value,
) -> Value {
    let response = Client::connect(settings.socket())
        .unwrap()
        .call_current_as_operator(method, Some(params), credential(settings))
        .unwrap();
    assert!(response.error.is_none(), "{response:?}");
    response.result.unwrap()
}

/// The operator consents to repeat the owed first turn after a move, leaving the claim pending
/// because the new folder's instructions are still to be reviewed.
fn consent_to_repeat(crashed: &mut super::CrashAfterMove) {
    use dudes_proto::rpc::CurrentMethod;
    use dudes_proto::rpc::recovery::{
        RecoveryAllowedChoices, RecoveryChoice, RecoveryInspectParams, RecoveryInspectResult,
        RecoverySubmitParams, RecoverySubmitResult,
    };
    crashed
        .revived
        .await_status(crashed.docs_card, CardStatus::Blocked);
    assert!(crashed.revived.host.spawned().is_empty());
    let inspected: RecoveryInspectResult = serde_json::from_value(recovery_call(
        &crashed.settings,
        CurrentMethod::RecoveryInspect,
        serde_json::to_value(RecoveryInspectParams { agent: cody() }).unwrap(),
    ))
    .unwrap();
    let RecoveryInspectResult::Available {
        expected,
        reason,
        choices,
        waiting,
        ..
    } = inspected
    else {
        panic!("the owed issue offers recovery: {inspected:?}");
    };
    assert_eq!(reason, dudes_proto::recovery::RecoveryReason::MissingSpawn);
    assert_eq!(choices, RecoveryAllowedChoices::RepeatIssuedFresh);
    assert_eq!(
        waiting.instruction_review,
        dudes_proto::rpc::recovery::RecoveryWaitStatus::Waiting,
        "the new folder's instructions are still to be reviewed"
    );
    let submitted: RecoverySubmitResult = serde_json::from_value(recovery_call(
        &crashed.settings,
        CurrentMethod::RecoverySubmit,
        serde_json::to_value(RecoverySubmitParams {
            request_id: dudes_proto::record::Hex32::new([7; 32]),
            expected: *expected,
            choice: RecoveryChoice::RepeatIssuedFresh,
        })
        .unwrap(),
    ))
    .unwrap();
    assert!(matches!(submitted, RecoverySubmitResult::Accepted { .. }));
}

#[test]
fn consent_after_a_crash_in_the_first_turn_after_a_move_runs_the_turn_in_the_new_folder() {
    let mut crashed = super::crash_after_a_move();
    consent_to_repeat(&mut crashed);
    let goal = crashed
        .revived
        .read(crashed.docs_card)
        .ticket
        .goal
        .expect("the ticket holds its goal");
    crashed
        .revived
        .acknowledge_unfinished_review(crashed.docs_card, cody(), goal);
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while crashed.revived.host.spawned().is_empty() && std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(spawn_cwds(&crashed.revived), [crashed.docs.clone()]);
    crashed
        .revived
        .await_status(crashed.docs_card, CardStatus::Doing);
    assert_eq!(moved_agents(&crashed.second).len(), 1);
    let settings = crashed.settings.clone();
    let repo = crashed.repo.clone();
    let (_first, second) = crashed.stop();
    assert_projection_replays(&second);
    restart_accepts(&second, &settings, &repo);
}
