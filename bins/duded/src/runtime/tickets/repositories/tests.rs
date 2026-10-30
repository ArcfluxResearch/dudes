use dudes_proto::record::Hex32;
use dudes_proto::repository::{RepoId, TicketDeclaration};
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::ticket::{TicketCreateResult, TicketReadResult};
use dudes_proto::rpc::{Method, Response};
use dudes_proto::{Predicate, Record};
use dudes_rpc::Client;
use dudes_types::{AgentId, CardId};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::floor_actions::EnrolledCheck;
use crate::runtime::repository_hire_tests::Setup;
use crate::runtime::work_test_support as common;

type Socket =
    Client<std::io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream>;

fn setup() -> Setup {
    let mut setup = Setup::new();
    setup.settings.enrollment.profiles[0].checks = ["api", "web", "docs"]
        .into_iter()
        .map(|repo| EnrolledCheck {
            id: format!("{repo}-check"),
            label: format!("Check {repo}"),
            predicate: Predicate::Command {
                argv: vec![common::true_program().into()],
                timeout_s: 1,
            },
            may_already_pass: true,
            check_repo: Some(RepoId::parse(repo).unwrap()),
        })
        .collect();
    setup
}

fn call(socket: &mut Socket, setup: &Setup, method: Method, params: Value) -> Response {
    let credential = OperatorCredential::new(
        std::fs::read_to_string(
            setup
                .settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .unwrap(),
    )
    .unwrap();
    socket
        .call_as_operator(method, Some(params), credential)
        .unwrap()
}

fn ok<T: DeserializeOwned>(socket: &mut Socket, setup: &Setup, method: Method, params: Value) -> T {
    let response = call(socket, setup, method, params);
    assert!(response.error.is_none(), "{method:?}: {:?}", response.error);
    serde_json::from_value(response.result.unwrap()).unwrap()
}

fn read(socket: &mut Socket, setup: &Setup, card: CardId) -> TicketReadResult {
    ok(socket, setup, Method::TicketRead, json!({"card": card}))
}

fn declaration(read: &TicketReadResult) -> &TicketDeclaration {
    &read.ticket.repositories.declared().unwrap().declaration
}

fn names(declaration: &TicketDeclaration) -> Vec<&str> {
    declaration
        .repositories
        .entries()
        .iter()
        .map(|entry| entry.repo_id.as_str())
        .collect()
}

fn refused(socket: &mut Socket, setup: &Setup, method: Method, params: Value, message: &str) {
    let before = setup.fixture.records();
    let response = call(socket, setup, method, params);
    let error = response.error.expect("request must be refused");
    assert!(error.message.contains(message), "{error:?}");
    assert_eq!(setup.fixture.records(), before, "a refusal must not append");
}

fn create_and_edit(socket: &mut Socket, setup: &Setup) -> (CardId, CardId, TicketDeclaration) {
    let api_revision = setup.settings.enrollment.checks_view().workspaces[0].checks[0].revision;
    let all: TicketCreateResult = ok(
        socket,
        setup,
        Method::TicketCreate,
        json!({
            "title": "All three", "done_when_revision": api_revision, "check_repo": "api"
        }),
    );
    let all_read = read(socket, setup, all.card);
    assert_eq!(names(declaration(&all_read)), ["api", "web", "docs"]);
    assert_eq!(
        declaration(&all_read).repositories.primary_id().as_str(),
        "api"
    );
    assert_eq!(
        all_read
            .ticket
            .repositories
            .declared()
            .unwrap()
            .check_repo
            .as_str(),
        "api"
    );
    let subset: TicketCreateResult = ok(
        socket,
        setup,
        Method::TicketCreate,
        json!({
            "title": "Two repos", "repos": ["api", "web"], "check_repo": "api"
        }),
    );
    let before_edit = read(socket, setup, subset.card);
    let original = declaration(&before_edit).clone();
    assert_eq!(names(&original), ["api", "web"]);
    assert_eq!(original.repositories.primary_id().as_str(), "api");
    let _: Value = ok(
        socket,
        setup,
        Method::TicketEdit,
        json!({
            "card": subset.card, "title": "Edited", "done_when": "web-check", "check_repo": "web",
            "expected_seq": before_edit.card.seq
        }),
    );
    let edited = read(socket, setup, subset.card);
    assert_eq!(declaration(&edited), &original);
    assert_eq!(
        edited
            .ticket
            .repositories
            .declared()
            .unwrap()
            .check_repo
            .as_str(),
        "web"
    );
    let _: Value = ok(
        socket,
        setup,
        Method::TicketEdit,
        json!({
            "card": subset.card, "description": "Exact saved check", "expected_seq": edited.card.seq
        }),
    );
    (all.card, subset.card, original)
}

fn cold_replay_stays_closed(
    setup: &Setup,
    agent: AgentId,
    card: CardId,
    original: &TicketDeclaration,
) {
    let daemon = setup.boot(false);
    assert!(!daemon.ctx.repository_allowed(daemon.sup.world(), agent));
    let (_, replayed) = daemon.ticket_row(card).unwrap();
    assert_eq!(
        &replayed.repositories.declared().unwrap().declaration,
        original
    );
    let mut running = common::start(daemon);
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let current = read(&mut socket, setup, card);
    refused(
        &mut socket,
        setup,
        Method::TicketCreate,
        json!({"title": "Still dormant"}),
        "no verified provider capability",
    );
    refused(
        &mut socket,
        setup,
        Method::TicketEdit,
        json!({"card": card, "title": "Refused", "expected_seq": current.card.seq}),
        "no verified provider capability",
    );
    refused(
        &mut socket,
        setup,
        Method::TicketAssign,
        json!({"card": card, "agent": agent, "expected_seq": current.card.seq}),
        "no verified provider capability",
    );
    drop(socket);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn three_repository_writer_freezes_selection_edits_and_assigns_after_equivalent_regrant() {
    let setup = setup();
    let (mut running, hires, params) = setup.start(true);
    let agent = hires.hire(params).unwrap().agent;
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let (all, subset, original) = create_and_edit(&mut socket, &setup);
    // An unapplied operator-file edit cannot replace installed project or check authority.
    let operator_path = setup.settings.operator_config.as_ref().unwrap();
    let original_config = std::fs::read(operator_path).unwrap();
    std::fs::write(operator_path, "not valid TOML = [").unwrap();
    let trust_before = setup
        .fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::RepositoryTrustSet { .. }))
        .count();
    for name in ["web", "api", "docs"] {
        hires
            .trust(&dudes_proto::rpc::workspace::TrustParams {
                workspace_id: "service".into(),
                repo_id: Some(RepoId::parse(name).unwrap()),
                trusted: true,
            })
            .unwrap();
    }
    assert_eq!(
        setup
            .fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::RepositoryTrustSet { .. }))
            .count(),
        trust_before + 3
    );
    for card in [all, subset] {
        let current = read(&mut socket, &setup, card);
        let _: Value = ok(
            &mut socket,
            &setup,
            Method::TicketAssign,
            json!({"card": card, "agent": agent, "expected_seq": current.card.seq}),
        );
        assert_eq!(read(&mut socket, &setup, card).card.claimed_by, Some(agent));
    }
    std::fs::write(operator_path, original_config).unwrap();
    drop(socket);
    running.stop(crate::ExitCause::Ended);
    let records = setup.fixture.records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::TicketCreatedWithRepositories(_)))
            .count(),
        2
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::TicketEditedWithRepositories(_)))
            .count(),
        2
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::TicketAssigned { .. }))
            .count(),
        2
    );
    assert!(
        !records
            .iter()
            .any(|record| record.ticket_start().is_some() || record.turn_issue().is_some())
    );
    assert!(setup.host.spawned().is_empty());
    cold_replay_stays_closed(&setup, agent, subset, &original);
}

#[test]
fn scoped_ticket_start_freezes_default_or_subset_and_the_configured_owned_check_root() {
    for selection in [None, Some(json!(["api", "web"]))] {
        let setup = setup();
        let (mut running, hires, params) = setup.start(true);
        let agent = hires.hire(params).unwrap().agent;
        let mut socket = Client::connect(setup.settings.socket()).unwrap();
        let mut request = json!({"title":"Run the configured check", "check_repo":"api"});
        if let Some(selection) = selection {
            request["repos"] = selection;
        }
        let created: TicketCreateResult = ok(&mut socket, &setup, Method::TicketCreate, request);
        let queued = read(&mut socket, &setup, created.card);
        let declaration = declaration(&queued).clone();
        let _: Value = ok(
            &mut socket,
            &setup,
            Method::TicketAssign,
            json!({
                "card":created.card, "agent":agent, "expected_seq":queued.card.seq
            }),
        );
        let current = read(&mut socket, &setup, created.card);
        assert_eq!(
            current.card.status,
            dudes_types::CardStatus::Doing,
            "{:?}",
            current.ticket.reason
        );
        drop(socket);
        running.stop(crate::ExitCause::Ended);
        let records = setup.fixture.records();
        let entries = setup.fixture.entries();
        let (position, recorded) = entries
            .iter()
            .find_map(|(position, entry)| match &entry.record {
                Record::AgentHiredWithRepositories(recorded) if recorded.id == agent => {
                    Some((*position, recorded))
                }
                _ => None,
            })
            .unwrap();
        let starts: Vec<_> = records
            .iter()
            .filter_map(|record| match record {
                Record::TicketStartedWithRepositories(start) => Some(start),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 1);
        let start = starts[0];
        assert_eq!(start.repositories.declaration, declaration);
        assert_eq!(start.repositories.hired_lsn, position);
        assert_eq!(start.round, 1);
        assert_eq!(start.expected_seq, dudes_types::Seq(1));
        assert_eq!(start.repositories.check_repo.as_str(), "api");
        assert_eq!(
            start.repositories.check_root,
            recorded
                .repositories
                .repositories
                .get(&start.repositories.check_repo)
                .unwrap()
                .execution
        );
        assert_eq!(
            start.goal.completion_check,
            crate::ticket_index::goal_check(&start.repositories.check).map(|reference| {
                crate::ticket_index::scoped_check_reference(
                    "service",
                    &start.repositories.check_repo,
                    reference,
                )
            })
        );
        assert!(!records.iter().any(|record| record.turn_issue().is_some()
            || matches!(record, Record::TicketBaselineRecorded { .. })));
        assert!(setup.host.spawned().is_empty());
    }
}

#[test]
fn scoped_ticket_refusals_do_not_append_or_redirect_the_configured_check() {
    let setup = setup();
    let (mut running, _, _) = setup.start(true);
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    for (fields, message) in [
        (json!({"repos": []}), "nonempty and unique"),
        (json!({"repos": ["api", "api"]}), "nonempty and unique"),
        (json!({"repos": ["missing"]}), "unknown member"),
        (json!({"repos": ["docs"]}), "outside the declaration"),
        (json!({"check_repo": "web"}), "changed"),
        (
            json!({"done_when_revision": Hex32::new([0; 32])}),
            "changed",
        ),
        (json!({"priority": "high"}), "does not support priority"),
    ] {
        let mut params = json!({"title": "Refused"});
        params
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        refused(&mut socket, &setup, Method::TicketCreate, params, message);
    }
    let created: TicketCreateResult = ok(
        &mut socket,
        &setup,
        Method::TicketCreate,
        json!({"title": "Kept", "repos": ["api", "web"]}),
    );
    let current = read(&mut socket, &setup, created.card);
    for (fields, message) in [
        (
            json!({"done_when": "docs-check"}),
            "outside the immutable declaration",
        ),
        (
            json!({"title": "Priority", "priority": "none"}),
            "do not support priority",
        ),
        (
            json!({"done_when": "web-check", "check_repo": "api"}),
            "changed",
        ),
    ] {
        let mut params = json!({"card": created.card, "expected_seq": current.card.seq});
        params
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        refused(&mut socket, &setup, Method::TicketEdit, params, message);
    }
    assert_eq!(
        declaration(&read(&mut socket, &setup, created.card)),
        declaration(&current)
    );
    drop(socket);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn assignment_refuses_revoked_equivalence_and_missing_copy() {
    let setup = setup();
    let (mut running, hires, params) = setup.start(true);
    let agent = hires.hire(params).unwrap().agent;
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let created: TicketCreateResult = ok(
        &mut socket,
        &setup,
        Method::TicketCreate,
        json!({"title": "Kept", "repos": ["api", "web"]}),
    );
    let current = read(&mut socket, &setup, created.card);
    hires
        .trust(&dudes_proto::rpc::workspace::TrustParams {
            workspace_id: "service".into(),
            repo_id: Some(RepoId::parse("docs").unwrap()),
            trusted: false,
        })
        .unwrap();
    let params = json!({"card": created.card, "agent": agent, "expected_seq": current.card.seq});
    refused(
        &mut socket,
        &setup,
        Method::TicketAssign,
        params.clone(),
        "trust grant does not match",
    );
    hires
        .trust(&dudes_proto::rpc::workspace::TrustParams {
            workspace_id: "service".into(),
            repo_id: Some(RepoId::parse("docs").unwrap()),
            trusted: true,
        })
        .unwrap();
    let records = setup.fixture.records();
    let copy = records
        .iter()
        .find_map(|record| match record {
            Record::AgentHiredWithRepositories(hire) => {
                Some(&hire.repositories.repositories.primary().execution.directory)
            }
            _ => None,
        })
        .unwrap();
    let moved = copy.with_extension("moved");
    std::fs::rename(copy, &moved).unwrap();
    refused(
        &mut socket,
        &setup,
        Method::TicketAssign,
        params,
        "copy is unavailable",
    );
    std::fs::rename(moved, copy).unwrap();
    drop(socket);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn scoped_check_revision_binds_context_and_full_definition_but_legacy_stays_exact() {
    let setup = setup();
    let profile = &setup.settings.enrollment.profiles[0];
    let original = profile.resolve_check(None).unwrap().reference().revision;
    assert_eq!(
        setup.settings.enrollment.checks_view().workspaces[0].checks[0].revision,
        original
    );
    let mut changed = profile.clone();
    changed.id = "another-project".into();
    assert_ne!(
        changed.resolve_check(None).unwrap().reference().revision,
        original
    );
    changed = profile.clone();
    changed.checks[0].check_repo = Some(RepoId::parse("web").unwrap());
    assert_ne!(
        changed.resolve_check(None).unwrap().reference().revision,
        original
    );
    changed = profile.clone();
    changed.checks[0].label.push_str(" changed");
    assert_ne!(
        changed.resolve_check(None).unwrap().reference().revision,
        original
    );
    changed = profile.clone();
    changed.checks[0].predicate = Predicate::Command {
        argv: vec![common::true_program().into(), "changed argument".into()],
        timeout_s: 2,
    };
    assert_ne!(
        changed.resolve_check(None).unwrap().reference().revision,
        original
    );
    changed = profile.clone();
    changed.checks[0].may_already_pass = false;
    assert_ne!(
        changed.resolve_check(None).unwrap().reference().revision,
        original
    );
    changed = profile.clone();
    changed.checks[0].check_repo = None;
    assert_eq!(
        changed
            .resolve_check(None)
            .unwrap()
            .repo()
            .unwrap()
            .as_str(),
        "api"
    );
    assert_eq!(
        changed.resolve_check(None).unwrap().reference().revision,
        original
    );
    changed.repositories = None;
    changed.checks[0].check_repo = None;
    let legacy = changed.checks[0].reference().unwrap();
    assert_eq!(changed.resolve_check(None).unwrap().reference(), &legacy);
    assert!(changed.resolve_check(None).unwrap().repo().is_none());
}

#[test]
fn scoped_ticket_start_rechecks_the_owned_check_root_after_its_final_permit() {
    let setup = setup();
    let (mut running, hires, params) = setup.start(true);
    let agent = hires.hire(params).unwrap().agent;
    running.stop(crate::ExitCause::Ended);
    let mut daemon = setup.boot(true);
    let by = dudes_proto::Actor::Peer { pid: 17, uid: None };
    let request =
        serde_json::from_value(json!({"title":"Keep the check root", "check_repo":"api"})).unwrap();
    let crate::ports::Answered::TicketCreated(created) =
        daemon.ticket_create(&request, None, by).unwrap()
    else {
        panic!("scoped ticket")
    };
    daemon
        .board_append(Record::TicketAssigned {
            card: created.card,
            agent,
            by,
            expected_seq: dudes_types::Seq::ZERO,
            priority: None,
            message: None,
        })
        .unwrap();
    let copy = match &daemon.sup.world().agents[&agent].repositories {
        dudes_proto::repository::HireRepositories::Scoped(hire) => hire
            .repositories
            .get(&RepoId::parse("api").unwrap())
            .unwrap()
            .recorded
            .execution
            .directory
            .clone(),
        dudes_proto::repository::HireRepositories::Legacy(_) => unreachable!(),
    };
    let pause = daemon.onboarding_pause();
    pause.arm(crate::runtime::OnboardingPausePoint::AfterTicketPermit);
    let worker = std::thread::spawn(move || {
        let outcome = daemon.ticket_reactions();
        (daemon, outcome)
    });
    assert!(pause.wait_reached(std::time::Duration::from_secs(2)));
    let preserved = copy.with_extension("preserved");
    std::fs::rename(&copy, &preserved).unwrap();
    std::fs::create_dir(&copy).unwrap();
    std::fs::write(copy.join("sentinel"), "keep").unwrap();
    pause.release();
    let (daemon, outcome) = worker.join().unwrap();
    assert!(outcome.is_err());
    assert!(
        !setup
            .fixture
            .records()
            .iter()
            .any(|record| record.ticket_start().is_some() || record.turn_issue().is_some())
    );
    assert_eq!(
        std::fs::read_to_string(copy.join("sentinel")).unwrap(),
        "keep"
    );
    assert!(setup.host.spawned().is_empty());
    drop(daemon);
}

#[test]
fn repository_origin_drift_at_the_final_ticket_permit_appends_nothing() {
    let setup = setup();
    let daemon = setup.boot(true);
    let pause = daemon.onboarding_pause();
    pause.arm(crate::runtime::OnboardingPausePoint::AfterTicketPermit);
    let mut running = common::start(daemon);
    let credential = OperatorCredential::new(
        std::fs::read_to_string(
            setup
                .settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .unwrap(),
    )
    .unwrap();
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let before = setup.fixture.records();
    let worker = std::thread::spawn(move || {
        socket
            .call_as_operator(
                Method::TicketCreate,
                Some(json!({"title": "No append"})),
                credential,
            )
            .unwrap()
    });
    assert!(pause.wait_reached(std::time::Duration::from_secs(2)));
    let origin = &setup.settings.enrollment.profiles[0]
        .repositories
        .as_ref()
        .unwrap()
        .primary()
        .path;
    let moved = origin.with_extension("preserved");
    std::fs::rename(origin, &moved).unwrap();
    std::fs::create_dir(origin).unwrap();
    std::fs::write(origin.join("sentinel"), "keep").unwrap();
    pause.release();
    let response = worker.join().unwrap();
    assert!(response.error.is_some(), "{response:?}");
    assert_eq!(setup.fixture.records(), before);
    assert_eq!(
        std::fs::read_to_string(origin.join("sentinel")).unwrap(),
        "keep"
    );
    assert!(setup.host.spawned().is_empty());
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn a_subset_request_is_refused_while_the_capability_is_closed_and_a_full_one_reaches_it() {
    let setup = setup();
    let (mut running, _, _) = setup.start(false);
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    refused(
        &mut socket,
        &setup,
        Method::TicketCreate,
        json!({"title": "Two of three", "repos": ["api", "web"]}),
        "covers every repository",
    );
    for repos in [json!(["docs", "web", "api"]), json!(null)] {
        let mut params = json!({"title": "All three"});
        if !repos.is_null() {
            params["repos"] = repos;
        }
        refused(
            &mut socket,
            &setup,
            Method::TicketCreate,
            params,
            "no verified provider capability",
        );
    }
    drop(socket);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn a_full_request_in_any_order_declares_the_primary_first_and_the_rest_in_catalog_order() {
    let setup = setup();
    let (mut running, _, _) = setup.start(true);
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    for repos in [None, Some(json!(["docs", "web", "api"]))] {
        let mut params = json!({"title": "All three"});
        if let Some(repos) = repos {
            params["repos"] = repos;
        }
        let created: TicketCreateResult = ok(&mut socket, &setup, Method::TicketCreate, params);
        let read = read(&mut socket, &setup, created.card);
        assert_eq!(names(declaration(&read)), ["api", "web", "docs"]);
        assert_eq!(declaration(&read).repositories.primary_id().as_str(), "api");
    }
    drop(socket);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn an_unconfined_hire_refuses_a_narrower_declaration_at_assignment_and_at_start() {
    let setup = setup();
    let (mut running, hires, params) = setup.start(true);
    let agent = hires.hire(params).unwrap().agent;
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let mut cards = Vec::new();
    for repos in [None, Some(json!(["api", "web"]))] {
        let mut params = json!({"title": "Cover", "check_repo": "api"});
        if let Some(repos) = repos {
            params["repos"] = repos;
        }
        let created: TicketCreateResult = ok(&mut socket, &setup, Method::TicketCreate, params);
        cards.push(created.card);
    }
    drop(socket);
    running.stop(crate::ExitCause::Ended);

    let daemon = setup.boot(true);
    let mut world = daemon.sup.world().clone();
    let row = world.agents.get_mut(&agent).unwrap();
    let dudes_proto::repository::HireRepositories::Scoped(hire) = &mut row.repositories else {
        panic!("scoped hire")
    };
    hire.policy = dudes_proto::repository::RepositoryPolicy::OperatorTrustV1;
    let check_root = hire
        .repositories
        .get(&RepoId::parse("api").unwrap())
        .unwrap()
        .recorded
        .execution
        .clone();
    let hired_lsn = row.mcp.hire_lsn;
    let admission = crate::runtime::repository_admission::CurrentHireAdmission::new(
        &daemon.ctx.enrollment,
        &daemon.ctx.current_repositories,
    );
    for (card, narrower) in [(cards[0], false), (cards[1], true)] {
        let (_, ticket) = daemon.ticket_row(card).unwrap();
        let scope = ticket.repositories.declared().unwrap();
        let assigned = Record::TicketAssigned {
            card,
            agent,
            by: dudes_proto::Actor::System,
            message: None,
            expected_seq: dudes_types::Seq::ZERO,
            priority: None,
        };
        let check = scope.declaration.clone();
        let started = Record::TicketStartedWithRepositories(Box::new(
            dudes_proto::repository::RepositoryTicketStarted {
                card,
                agent,
                expected_seq: dudes_types::Seq::ZERO,
                round: 1,
                goal: dudes_proto::Goal {
                    id: dudes_types::GoalId::from_parts(8, 1),
                    text: "Cover".into(),
                    predicate: ticket.done_when.predicate.clone(),
                    baseline: None,
                    completion_check: None,
                    budget: dudes_proto::GoalBudget {
                        usd: None,
                        tokens: None,
                    },
                    hop_cap: 4,
                },
                repositories: dudes_proto::repository::TicketStartRepositories {
                    hired_lsn,
                    declaration: check,
                    check: ticket.done_when.clone(),
                    check_repo: scope.check_repo.clone(),
                    check_root: check_root.clone(),
                },
            },
        ));
        for record in [&assigned, &started] {
            let result = admission.require_ticket_append(&world, record, Some(&ticket));
            if narrower {
                let message = result.unwrap_err().to_string();
                assert!(message.contains("covers every repository"), "{message}");
            } else {
                assert!(result.is_ok(), "{result:?}");
            }
        }
    }
}

fn assigned_unconfined_ticket(setup: &Setup, agent: AgentId, socket: &mut Socket) -> CardId {
    let created: TicketCreateResult = ok(
        socket,
        setup,
        Method::TicketCreate,
        json!({"title": "Cover all", "check_repo": "api"}),
    );
    let queued = read(socket, setup, created.card);
    let _: Value = ok(
        socket,
        setup,
        Method::TicketAssign,
        json!({"card": created.card, "agent": agent, "expected_seq": queued.card.seq}),
    );
    created.card
}

#[test]
fn an_unconfined_ticket_turn_launches_in_the_primary_copy_with_the_others_added() {
    let mut setup = setup();
    setup
        .settings
        .daemon_env
        .push(dudes_providers::launch::EnvVar::new(
            "CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD",
            "1",
        ));
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let card = assigned_unconfined_ticket(&setup, agent, &mut socket);
    setup.fixture.await_record("the ticket turn", |record| {
        matches!(record, Record::TurnSpawned { .. }).then_some(())
    });
    assert_eq!(
        read(&mut socket, &setup, card).card.status,
        dudes_types::CardStatus::Doing
    );
    drop(socket);
    running.stop(crate::ExitCause::Ended);
    let spawned = setup.host.spawned();
    assert_eq!(spawned.len(), 1);
    setup.assert_primary_with_others_added(&spawned[0], agent);
    let argv: Vec<_> = spawned[0]
        .argv
        .iter()
        .map(|part| part.to_string_lossy())
        .collect();
    assert_eq!(
        argv.iter().filter(|part| *part == "--add-dir").count(),
        3,
        "the second and third copy, and the chat-files folder: {argv:?}"
    );
    assert!(
        argv.iter()
            .any(|part| part == "--dangerously-skip-permissions"),
        "the operator's permission setting, as for a single-repo hire: {argv:?}"
    );
    assert!(
        !spawned[0]
            .env
            .iter()
            .any(|var| var.name() == "CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD"),
        "the memory-file switch reached the child"
    );
    let records = setup.fixture.records();
    assert!(
        records
            .iter()
            .any(|record| matches!(record, Record::TicketStartedWithRepositories(_)))
    );
}

#[test]
fn an_unconfined_ticket_whose_second_copy_goes_missing_before_its_turn_holds_the_agent() {
    let setup = setup();
    let mut daemon = setup.boot_unconfined();
    let vanishing = std::sync::Arc::new(std::sync::Mutex::new(None::<std::path::PathBuf>));
    let hook = vanishing.clone();
    daemon.ctx.before_launch_copy_check = Some(Box::new(move || {
        if let Some(copy) = hook.lock().unwrap().take() {
            std::fs::rename(&copy, copy.with_extension("moved")).unwrap();
        }
    }));
    let (mut running, hires, params) = setup.start_from(daemon);
    let agent = hires.hire(params).unwrap().agent;
    *vanishing.lock().unwrap() = Some(setup.copies_of(agent)[1].clone());
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let card = assigned_unconfined_ticket(&setup, agent, &mut socket);
    let worktree = setup
        .fixture
        .await_record("the hold", |record| match record {
            Record::AgentHeld {
                agent: held,
                cause: dudes_proto::AutomaticHold::WorktreeMissing { worktree },
                ..
            } if *held == agent => Some(*worktree),
            _ => None,
        });
    let status = read(&mut socket, &setup, card).card.status;
    drop(socket);
    running.stop(crate::ExitCause::Ended);
    assert!(
        setup
            .fixture
            .records()
            .iter()
            .any(|record| record.worktree_lease().is_some_and(
                |lease| lease.worktree == worktree && lease.path == setup.copies_of(agent)[1]
            ))
    );
    assert_eq!(status, dudes_types::CardStatus::Doing);
    assert!(setup.host.spawned().is_empty());
}

#[test]
fn assigning_a_ticket_to_an_unconfined_agent_with_a_missing_copy_is_refused() {
    let setup = setup();
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let created: TicketCreateResult = ok(
        &mut socket,
        &setup,
        Method::TicketCreate,
        json!({"title": "Cover all", "check_repo": "api"}),
    );
    let queued = read(&mut socket, &setup, created.card);
    let copy = setup.copies_of(agent)[1].clone();
    std::fs::rename(&copy, copy.with_extension("moved")).unwrap();
    refused(
        &mut socket,
        &setup,
        Method::TicketAssign,
        json!({"card": created.card, "agent": agent, "expected_seq": queued.card.seq}),
        "copy is unavailable",
    );
    drop(socket);
    running.stop(crate::ExitCause::Ended);
    assert!(setup.host.spawned().is_empty());
}

#[test]
fn a_queued_ticket_for_an_unconfined_agent_whose_second_copy_is_gone_holds_it_before_any_turn() {
    let setup = setup();
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    assigned_unconfined_ticket(&setup, agent, &mut socket);
    let first = setup
        .fixture
        .await_record("the first ticket turn", |record| match record {
            Record::TurnSpawned { turn, .. } => Some(*turn),
            _ => None,
        });
    let second = assigned_unconfined_ticket(&setup, agent, &mut socket);
    let copy = setup.copies_of(agent)[1].clone();
    std::fs::rename(&copy, copy.with_extension("moved")).unwrap();
    let issued_before = issued(&setup);
    common::end_turn(&running.handle, first);
    setup
        .fixture
        .await_record("the hold", |record| match record {
            Record::AgentHeld {
                agent: held,
                cause: dudes_proto::AutomaticHold::WorktreeMissing { .. },
                ..
            } if *held == agent => Some(()),
            _ => None,
        });
    let status = read(&mut socket, &setup, second).card.status;
    drop(socket);
    running.stop(crate::ExitCause::Ended);
    assert_ne!(status, dudes_types::CardStatus::Doing);
    assert_eq!(
        issued(&setup),
        issued_before,
        "no turn was issued for the queued ticket"
    );
    assert_eq!(setup.host.spawned().len(), 1);
}

fn issued(setup: &Setup) -> usize {
    setup
        .fixture
        .records()
        .iter()
        .filter(|record| format!("{:?}", record.kind()).contains("Issued"))
        .count()
}

fn finish_the_round(
    setup: &Setup,
    running: &common::Running,
    socket: &mut Socket,
    card: CardId,
) -> TicketReadResult {
    let turn = setup
        .fixture
        .await_record("the ticket turn", |record| match record {
            Record::TurnSpawned { turn, .. } => Some(*turn),
            _ => None,
        });
    common::reap_native_child(
        &setup.fixture,
        &setup.host,
        dudes_supervisor::Input::ChildExited {
            report: dudes_proto::TurnReport::default(),
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: dudes_proto::ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: None,
            model_usage: std::collections::BTreeMap::new(),
            outcome: dudes_proto::TurnOutcome::Completed,
            last_message: None,
        },
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let detail = read(socket, setup, card);
        let settled = matches!(
            detail.card.status,
            dudes_types::CardStatus::Review | dudes_types::CardStatus::Blocked
        ) && detail
            .ticket
            .instruction_review
            .as_ref()
            .is_none_or(|review| !review.pending_comparison);
        if settled || std::time::Instant::now() > deadline {
            return detail;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn accept(socket: &mut Socket, setup: &Setup, detail: &TicketReadResult) -> Response {
    let watermark = detail
        .ticket
        .instruction_review
        .as_ref()
        .map(|review| review.watermark);
    call(
        socket,
        setup,
        Method::TicketAccept,
        json!({
            "card": detail.card.id,
            "expected_seq": detail.card.seq,
            "review_watermark": watermark,
        }),
    )
}

#[test]
fn an_unconfined_ticket_that_changes_nothing_reaches_ready_and_accepts() {
    let mut setup = setup();
    setup.host = common::SharedHost::with_native_evidence(setup.fixture.journal_dir());
    setup.own_children = true;
    let profile = setup.fixture.runtime.path().join("profile");
    std::fs::create_dir_all(profile.join(".claude")).unwrap();
    std::fs::write(profile.join(".claude/CLAUDE.md"), "profile instructions").unwrap();
    setup
        .settings
        .daemon_env
        .push(dudes_providers::launch::EnvVar::new(
            "HOME",
            profile.to_str().unwrap(),
        ));
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let card = assigned_unconfined_ticket(&setup, agent, &mut socket);
    let detail = finish_the_round(&setup, &running, &mut socket, card);
    assert_eq!(detail.card.status, dudes_types::CardStatus::Review);
    let review = detail.ticket.instruction_review.as_ref();
    assert!(
        review.is_none_or(|review| !review.coverage_unknown && review.changed_paths.is_empty()),
        "{review:?}"
    );
    let accepted = accept(&mut socket, &setup, &detail);
    assert!(accepted.error.is_none(), "{:?}", accepted.error);
    assert_eq!(
        read(&mut socket, &setup, card).card.status,
        dudes_types::CardStatus::Done
    );
    drop(socket);
    running.stop(crate::ExitCause::Ended);
}
