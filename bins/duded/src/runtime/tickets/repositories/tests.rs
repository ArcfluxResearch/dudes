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
    assert_eq!(names(declaration(&all_read)), ["web", "api", "docs"]);
    assert_eq!(
        declaration(&all_read).repositories.primary_id().as_str(),
        "web"
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
    assert_eq!(names(&original), ["web", "api"]);
    assert_eq!(original.repositories.primary_id().as_str(), "web");
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
