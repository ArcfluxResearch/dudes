use super::*;
use dudes_config::workspace::{NewWorkspace, RepositoryDefinition};
use dudes_proto::rpc::Method;

#[path = "current_enrollment/authority_regressions.rs"]
mod authority_regressions;

fn configured(
    fixture: &Fixture,
    origins: &[RepositoryEntry<CatalogRepository>],
) -> (duded::Settings, PathBuf) {
    let config = fixture.runtime.path().join("operator.toml");
    let new = NewWorkspace::with_defaults(
        origins[0].value.origin.directory.clone(),
        "service".into(),
        "Service".into(),
        dudes_config::check::changed_check(),
    );
    dudes_config::workspace::add(&config, &new).unwrap();
    let repositories = RepositorySet::new(
        origins[1].repo_id.clone(),
        origins
            .iter()
            .map(|entry| RepositoryEntry {
                repo_id: entry.repo_id.clone(),
                value: RepositoryDefinition {
                    label: entry.value.label.clone(),
                    path: entry.value.origin.directory.clone(),
                },
            })
            .collect(),
    )
    .unwrap();
    dudes_config::workspace::set_repositories(&config, "service", &repositories).unwrap();
    let operator = duded::config::load_operator(&config).unwrap();
    let mut settings = fixture.settings();
    settings.enrollment = operator.enrollment;
    settings.operator_config = Some(config.clone());
    (settings, config)
}

fn call(
    settings: &duded::Settings,
    method: Method,
    params: serde_json::Value,
) -> dudes_proto::rpc::Response {
    Client::connect(settings.socket())
        .unwrap()
        .call_as_operator(method, Some(params), credential(settings))
        .unwrap()
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one live fixture proves config order, trust and pre-lease refusal across warm replay"
)]
fn three_existing_repositories_round_trip_trust_and_refuse_every_cwd_hire_before_leasing() {
    let fixture = Fixture::new();
    let roots = tempfile::tempdir().unwrap();
    let root = roots.path().canonicalize().unwrap();
    let origins: Vec<_> = ["web", "api", "docs"]
        .iter()
        .map(|name| create_origin(&root, name))
        .collect();
    let (settings, config) = configured(&fixture, &origins);
    let host = SharedHost::new();
    let daemon = boot(
        &settings,
        common::seams(
            &settings,
            host.clone(),
            Arc::new(RealRunner(OneShot::new())),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let observed = daemon.world().repository_projects["service"]
        .revisions
        .clone();
    let catalog = observed.values().next().unwrap();
    assert_eq!(catalog.primary_id(), &origins[1].repo_id);
    assert_eq!(catalog.entries(), origins);
    assert_eq!(
        dudes_config::workspace::directory(&config, "service")
            .unwrap_err()
            .to_string(),
        "this project has explicit repositories; scoped repository execution is not available yet"
    );
    let mut running = common::start(daemon);
    assert!(
        call(
            &settings,
            Method::WorkspaceTrust,
            json!({"workspace_id":"service","trusted":true})
        )
        .error
        .is_some()
    );
    for origin in &origins {
        let result = call(
            &settings,
            Method::WorkspaceTrust,
            json!({"workspace_id":"service","repo_id":origin.repo_id,"trusted":true}),
        );
        assert!(result.error.is_none(), "{:?}", result.error);
        for provider in [
            dudes_types::Provider::Claude,
            dudes_types::Provider::Codex,
            dudes_types::Provider::Pi,
        ] {
            let mut params = common::hire_params(&origin.value.origin.directory);
            params.provider = provider;
            params.checkout = Some(dudes_proto::rpc::agent::Checkout::Worktree);
            params.trusted = true;
            assert!(
                running
                    .handle
                    .ask(duded::ports::Ask::Hire(Box::new(params)))
                    .is_err()
            );
        }
    }
    let grants: Vec<_> = fixture
        .entries()
        .into_iter()
        .filter_map(|(_, entry)| match entry.record {
            Record::RepositoryTrustSet {
                repo_id,
                origin,
                pin: Some(_),
                ..
            } => Some((repo_id, origin)),
            _ => None,
        })
        .collect();
    assert_eq!(grants.len(), 3);
    assert!(fixture.entries().iter().all(|(_, entry)| !matches!(
        entry.record,
        Record::WorktreeLeased { .. }
            | Record::AgentHired { .. }
            | Record::AgentHiredWithRepositories(_)
    )));
    assert!(host.spawned().is_empty());
    assert!(
        call(
            &settings,
            Method::WorkspaceTrust,
            json!({"workspace_id":"service","repo_id":"api","trusted":false})
        )
        .error
        .is_none()
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    let daemon = boot(
        &settings,
        common::seams(
            &settings,
            host.clone(),
            Arc::new(RealRunner(OneShot::new())),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    assert_eq!(
        daemon.world().repository_projects["service"].revisions,
        observed
    );
    assert!(
        daemon.world().repository_trust[&("service".into(), RepoId::parse("api").unwrap())]
            .pin
            .is_none()
    );
    let mut running = common::start(daemon);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn invalid_repository_shapes_and_identity_aliases_cannot_be_observed() {
    let fixture = Fixture::new();
    let roots = tempfile::tempdir().unwrap();
    let root = roots.path().canonicalize().unwrap();
    let origins: Vec<_> = ["web", "api", "docs"]
        .iter()
        .map(|name| create_origin(&root, name))
        .collect();
    let (settings, config) = configured(&fixture, &origins);
    let original = std::fs::read_to_string(&config).unwrap();
    for invalid in [
        original.replace("primary_repo = \"api\"", "primary_repo = \"absent\""),
        original.replace("repo_id = \"api\"", "repo_id = \"web\""),
        format!("{original}\n[[floor.onboarding.workspaces]]\nworkspace_id = 'broken'\n"),
    ] {
        std::fs::write(&config, invalid).unwrap();
        assert!(duded::config::load_operator(&config).is_err());
    }
    std::fs::write(&config, original).unwrap();
    let mut aliased = settings;
    let repositories = aliased.enrollment.profiles[0]
        .repositories
        .as_mut()
        .unwrap();
    let mut entries = repositories.entries().to_vec();
    entries[1].value.path = entries[0].value.path.clone();
    entries[1].value.written_path = entries[0].value.written_path.clone();
    *repositories = RepositorySet::new(repositories.primary_id().clone(), entries).unwrap();
    let host = SharedHost::new();
    let daemon = boot(
        &aliased,
        common::seams(
            &aliased,
            host.clone(),
            Arc::new(RealRunner(OneShot::new())),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    assert!(daemon.world().repository_projects.is_empty());
    assert!(host.spawned().is_empty());
    let mut running = common::start(daemon);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn a_malformed_operator_reload_closes_admission_before_reply_and_an_unauthorized_read_cannot_pause()
{
    let fixture = Fixture::new();
    let roots = tempfile::tempdir().unwrap();
    let origin = create_origin(&roots.path().canonicalize().unwrap(), "web");
    let config = fixture.runtime.path().join("operator.toml");
    dudes_config::workspace::add(
        &config,
        &NewWorkspace::with_defaults(
            origin.value.origin.directory.clone(),
            "service".into(),
            "Service".into(),
            dudes_config::check::changed_check(),
        ),
    )
    .unwrap();
    let original = std::fs::read(&config).unwrap();
    let mut settings = fixture.settings();
    settings.enrollment = duded::config::load_operator(&config).unwrap().enrollment;
    settings.operator_config = Some(config.clone());
    let host = SharedHost::new();
    let daemon = boot(
        &settings,
        common::seams(
            &settings,
            host.clone(),
            Arc::new(RealRunner(OneShot::new())),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, &origin.value.origin.directory);
    std::fs::write(&config, "[invalid").unwrap();
    let before = fixture.entries();
    let unauthorized = Client::connect(settings.socket())
        .unwrap()
        .call(Method::FloorEnrollmentReload, Some(json!({})))
        .unwrap();
    assert!(unauthorized.error.is_some());
    assert_eq!(fixture.entries(), before);
    assert!(
        call(&settings, Method::FloorEnrollmentReload, json!({}))
            .error
            .is_some()
    );
    assert!(fixture.entries().iter().any(
        |(_, entry)| matches!(entry.record, Record::Pause { agent: who, .. } if who == agent)
    ));
    assert!(
        running
            .handle
            .ask(duded::ports::Ask::Resume(agent))
            .is_err()
    );
    assert!(host.spawned().is_empty());
    std::fs::write(&config, original).unwrap();
    let checked = call(&settings, Method::FloorEnrollmentReload, json!({}))
        .result
        .unwrap();
    let applied = call(
        &settings,
        Method::FloorEnrollmentReload,
        json!({"apply": {
            "sha256": checked["sha256"], "added": [], "removed": [], "changed": [],
        }}),
    );
    assert!(applied.error.is_none(), "{:?}", applied.error);
    assert_eq!(applied.result.unwrap()["applied"], true);
    assert!(running.handle.ask(duded::ports::Ask::Resume(agent)).is_ok());
    assert!(host.spawned().is_empty());
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn an_owed_pre_expansion_singleton_stays_frozen_and_unlaunched_with_warm_and_cold_current_enrollment()
 {
    let fixture = Fixture::new();
    let roots = tempfile::tempdir().unwrap();
    let root = roots.path().canonicalize().unwrap();
    let origins: Vec<_> = ["web", "api", "docs"]
        .iter()
        .map(|name| create_origin(&root, name))
        .collect();
    let (settings, _) = configured(&fixture, &origins);
    let agent = AgentId::from_ulid(ulid::Ulid::from(700_u128));
    let mut hire = legacy_hire(agent, &origins[0].value.origin.directory);
    let Record::AgentHired { workspace, .. } = &mut hire else {
        unreachable!()
    };
    *workspace = Some(RecordedWorkspace {
        id: "service".into(),
        label: "Service".into(),
    });
    let mut goal = samples::record(RecordKind::GoalSet);
    let Record::GoalSet { agent: owner, .. } = &mut goal else {
        unreachable!()
    };
    *owner = agent;
    let issue = legacy_issue(agent);
    let turn = issue.turn_issue().unwrap().turn;
    let (mut writer, _) = dudes_journal::open(&fixture.journal_dir(), settings.journal).unwrap();
    append(&mut writer, hire);
    append(&mut writer, goal);
    append(&mut writer, issue);
    drop(writer);
    let host = SharedHost::new();
    for cold in [false, false, true] {
        if cold {
            std::fs::remove_file(settings.state_dir.join(duded::boot::PROJECTION_FILE)).unwrap();
        }
        let daemon = boot(
            &settings,
            common::seams(
                &settings,
                host.clone(),
                Arc::new(RealRunner(OneShot::new())),
                Arc::new(TestClock::new()),
                Witness::default(),
            ),
        )
        .unwrap();
        let row = &daemon.world().agents[&agent];
        assert!(row.paused);
        let singleton = row.repositories.singleton().unwrap();
        assert_eq!(singleton.cwd(), origins[0].value.origin.directory);
        assert_eq!(singleton.workspace().unwrap().id, "service");
        assert!(daemon.world().turns[&turn].is_issued_not_spawned());
        assert!(host.spawned().is_empty());
        let mut running = common::start(daemon);
        assert!(
            running
                .handle
                .ask(duded::ports::Ask::Resume(agent))
                .is_err()
        );
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}
