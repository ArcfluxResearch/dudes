use super::*;
use common::PageHttp as _;
use serde_json::{Value, json};

fn floor_setup() -> Setup {
    let mut setup = Setup::with_repositories(&["web", "api"]);
    setup.settings.floor = Some(crate::settings::FloorSettings {
        assets_dir: std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/floor-release-v10"
        )),
        port: Some(0),
    });
    setup
}

fn request(params: &HireParams, attempt: char) -> Value {
    let target = params.project.as_ref().unwrap();
    json!({"attempt_id": format!("{attempt}{}", "A".repeat(42)), "name": "Kim", "role": "engineer",
        "model": "claude-sonnet-4-6", "project": {"project_id": target.project_id,
            "expected_revision": target.expected_revision.0.to_string(), "primary_repo": "api", "repo_ids": ["web", "api"]}})
}

fn page(setup: &Setup, address: std::net::SocketAddr) -> Arc<crate::floor_browser::Page> {
    Arc::new(common::floor_session_on(&setup.settings, address, || {
        1_787_394_600_000
    }))
}

#[test]
fn a_browser_group_pending_conflict_and_lost_response_retry_keep_one_atomic_hire() {
    let setup = floor_setup();
    let daemon = setup.boot(true);
    let address = daemon.floor_addr().unwrap();
    let (mut running, _, params) = setup.start_from(daemon);
    let page = page(&setup, address);
    let body = request(&params, 'A');
    let (reached, release) = running.handle.hold_after_copy();
    let owner = {
        let page = Arc::clone(&page);
        let body = body.clone();
        std::thread::spawn(move || page.post("/floor/onboard", &body))
    };
    reached.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(page.post("/floor/onboard", &body).status(), 202);
    let mut changed = body.clone();
    changed["model"] = json!("other-model");
    assert_eq!(page.post("/floor/onboard", &changed).status(), 409);
    release.send(()).unwrap();
    let first = owner.join().unwrap();
    assert_eq!(
        first.status(),
        200,
        "{}",
        String::from_utf8_lossy(&first.body)
    );
    assert_eq!(first.json()["project"], body["project"]);
    let retry = page.post("/floor/onboard", &body);
    assert_eq!(
        retry.status(),
        200,
        "{}",
        String::from_utf8_lossy(&retry.body)
    );
    assert_eq!(retry.json(), first.json());
    running.stop(ExitCause::Ended);
    let records = setup.fixture.records();
    assert_eq!(
        records
            .iter()
            .filter(|record| record.worktree_lease().is_some())
            .count(),
        2
    );
    let hires: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::AgentHiredWithRepositories(hire) => Some(hire),
            _ => None,
        })
        .collect();
    assert_eq!(hires.len(), 1);
    assert_eq!(hires[0].repositories.repositories.entries().len(), 2);
    assert!(hires[0].browser_onboarding.is_some());
    assert!(setup.host.spawned().is_empty());
}

#[test]
fn four_browser_groups_use_four_slots_and_a_recorded_retry_still_completes() {
    let setup = floor_setup();
    let daemon = setup.boot(true);
    let address = daemon.floor_addr().unwrap();
    let (mut running, _, params) = setup.start_from(daemon);
    let page = page(&setup, address);
    let mut first = Value::Null;
    for attempt in ['A', 'B', 'C', 'D'] {
        let response = page.post("/floor/onboard", &request(&params, attempt));
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        if attempt == 'A' {
            first = response.json();
        }
    }
    let before = setup.fixture.records().len();
    assert_eq!(
        page.post("/floor/onboard", &request(&params, 'E')).status(),
        403
    );
    assert_eq!(setup.fixture.records().len(), before);
    assert_eq!(
        page.post("/floor/onboard", &request(&params, 'A')).json(),
        first
    );
    running.stop(ExitCause::Ended);
    let mut replayed = setup.boot(false);
    let sessions = crate::floor_session::FloorSessions::new(
        "127.0.0.1:31337".parse().unwrap(),
        Arc::new(common::TestClock::new()),
    );
    let identity = sessions
        .admitted_for_test("127.0.0.1:31337", "http://127.0.0.1:31337")
        .0;
    let request = crate::floor_actions::OnboardRequest::parse(
        &serde_json::to_vec(&request(&params, 'A')).unwrap(),
    )
    .unwrap();
    let indexed = replayed
        .ctx
        .onboarding_index
        .attempt(request.attempt_id)
        .unwrap();
    assert_eq!(
        replayed
            .ctx
            .onboarding_index
            .count(indexed.metadata.session_digest),
        4
    );
    assert!(
        matches!(replayed.onboard(&identity, &request), crate::floor_actions::OnboardStep::Answer(Ok(crate::floor_actions::OnboardOutcome::ProjectComplete { agent, .. })) if agent.to_string() == first["agent"].as_str().unwrap())
    );
    assert_eq!(setup.fixture.records().len(), before);
}

#[test]
fn browser_group_revocation_at_the_final_permit_cleans_every_unassigned_copy() {
    let setup = floor_setup();
    let daemon = setup.boot(true);
    let address = daemon.floor_addr().unwrap();
    let authorizer = daemon.floor_session_authorizer().unwrap();
    let pause = daemon.onboarding_pause();
    let (mut running, _, params) = setup.start_from(daemon);
    let page = page(&setup, address);
    pause.arm(super::super::OnboardingPausePoint::BeforeHireAppend);
    let owner = {
        let page = Arc::clone(&page);
        let body = request(&params, 'A');
        std::thread::spawn(move || page.post("/floor/onboard", &body))
    };
    assert!(pause.wait_reached(Duration::from_secs(2)));
    assert!(authorizer.revoke_for_test(&page.cookie));
    pause.release();
    assert_eq!(owner.join().unwrap().status(), 403);
    running.stop(ExitCause::Ended);
    setup.assert_cleaned(2);
    let index = crate::floor_actions::OnboardingIndex::replay(
        &dudes_journal::Reader::open(&setup.fixture.journal_dir())
            .unwrap()
            .0,
    )
    .unwrap();
    assert!(
        index
            .attempt(crate::floor_actions::decode_attempt(&"A".repeat(43)).unwrap())
            .is_none()
    );
}

#[test]
fn closed_group_capability_offers_no_enabled_provider_and_appends_no_lease() {
    let setup = Setup::with_repositories(&["web", "api"]);
    let daemon = setup.boot(false);
    let offer = daemon
        .browser_repository_offer(&setup.settings.enrollment.profiles[0])
        .unwrap();
    assert!(
        offer
            .providers
            .iter()
            .all(|provider| !provider.enabled && provider.refusal.is_some())
    );
    let (mut running, _, params) = setup.start_from(daemon);
    let sessions = crate::floor_session::FloorSessions::new(
        "127.0.0.1:31337".parse().unwrap(),
        Arc::new(common::TestClock::new()),
    );
    let identity = sessions
        .admitted_for_test("127.0.0.1:31337", "http://127.0.0.1:31337")
        .0;
    let body = request(&params, 'A');
    let before = setup.fixture.records();
    assert!(
        running
            .handle
            .onboard(
                identity,
                crate::floor_actions::OnboardRequest::parse(&serde_json::to_vec(&body).unwrap())
                    .unwrap()
            )
            .is_err()
    );
    running.stop(ExitCause::Ended);
    assert_eq!(setup.fixture.records(), before);
}

#[test]
fn a_failed_browser_group_releases_its_attempt_and_counts_no_hire() {
    let setup = floor_setup();
    let daemon = setup.boot(true);
    let copies = Arc::clone(&daemon.ctx.predicates);
    let address = daemon.floor_addr().unwrap();
    let (mut running, _, params) = setup.start_from(daemon);
    let page = page(&setup, address);
    let body = request(&params, 'A');
    copies.after_add(2, true, || {});
    let failed = page.post("/floor/onboard", &body);
    assert_eq!(failed.status(), 409);
    assert_eq!(failed.json()["reason"], "repository_group");
    assert!(
        !setup
            .fixture
            .records()
            .iter()
            .any(|record| record.hire().is_some())
    );
    let retry = page.post("/floor/onboard", &body);
    assert_eq!(
        retry.status(),
        200,
        "{}",
        String::from_utf8_lossy(&retry.body)
    );
    running.stop(ExitCause::Ended);
    let records = setup.fixture.records();
    assert_eq!(
        records
            .iter()
            .filter(|record| record.hire().is_some())
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::WorktreeRemoved { .. }))
            .count(),
        2
    );
}

#[test]
fn concurrent_sessions_share_one_group_reservation_and_the_original_completion() {
    let setup = floor_setup();
    let daemon = setup.boot(true);
    let address = daemon.floor_addr().unwrap();
    let (mut running, _, params) = setup.start_from(daemon);
    let first_page = page(&setup, address);
    let other_page = page(&setup, address);
    let body = request(&params, 'A');
    let (reached, release) = running.handle.hold_after_copy();
    let first = {
        let body = body.clone();
        std::thread::spawn(move || first_page.post("/floor/onboard", &body))
    };
    reached.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(other_page.post("/floor/onboard", &body).status(), 202);
    release.send(()).unwrap();
    let retry = first.join().unwrap();
    assert_eq!(retry.status(), 200);
    assert_eq!(
        retry.json(),
        other_page.post("/floor/onboard", &body).json()
    );
    running.stop(ExitCause::Ended);
    let records = setup.fixture.records();
    assert_eq!(
        records
            .iter()
            .filter(|record| record.hire().is_some())
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record.worktree_lease().is_some())
            .count(),
        2
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::WorktreeRemoved { .. }))
            .count(),
        0
    );
}

#[test]
fn a_browser_group_durable_before_indexing_completes_after_restart_without_more_copies() {
    let setup = floor_setup();
    let daemon = setup.boot(true);
    let address = daemon.floor_addr().unwrap();
    let failure = daemon.onboarding_failure();
    let (mut running, _, params) = setup.start_from(daemon);
    let page = page(&setup, address);
    let body = request(&params, 'A');
    failure.arm(super::super::OnboardingFailurePoint::HireProjection);
    assert_eq!(page.post("/floor/onboard", &body).status(), 503);
    assert_eq!(page.post("/floor/onboard", &body).status(), 202);
    running.stop(ExitCause::Ended);
    let before = setup.fixture.records();
    let mut daemon = setup.boot(false);
    let sessions = crate::floor_session::FloorSessions::new(
        "127.0.0.1:31337".parse().unwrap(),
        Arc::new(common::TestClock::new()),
    );
    let identity = sessions
        .admitted_for_test("127.0.0.1:31337", "http://127.0.0.1:31337")
        .0;
    let request =
        crate::floor_actions::OnboardRequest::parse(&serde_json::to_vec(&body).unwrap()).unwrap();
    assert!(matches!(
        daemon.onboard(&identity, &request),
        crate::floor_actions::OnboardStep::Answer(Ok(
            crate::floor_actions::OnboardOutcome::ProjectComplete { .. }
        ))
    ));
    assert_eq!(setup.fixture.records(), before);
    let assigned = before
        .iter()
        .find_map(|record| match record {
            Record::AgentHiredWithRepositories(hire) => Some(hire),
            _ => None,
        })
        .unwrap();
    assert!(
        assigned
            .repositories
            .repositories
            .entries()
            .iter()
            .all(|entry| entry.value.execution.directory.exists())
    );
}
