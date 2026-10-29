use super::*;

type TestDaemon = duded::Daemon<SharedHost, RealRunner, TestClock, common::FixtureRng>;

fn open(settings: &duded::Settings, host: &SharedHost) -> TestDaemon {
    boot(
        settings,
        common::seams(
            settings,
            host.clone(),
            Arc::new(RealRunner(OneShot::new())),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap()
}

fn legacy_configured(fixture: &Fixture, directory: &Path) -> (duded::Settings, PathBuf) {
    let config = fixture.runtime.path().join("operator.toml");
    dudes_config::workspace::add(
        &config,
        &NewWorkspace::with_defaults(
            directory.to_path_buf(),
            "service".into(),
            "Service".into(),
            dudes_config::check::changed_check(),
        ),
    )
    .unwrap();
    let mut settings = settings(fixture);
    settings.enrollment = duded::config::load_operator(&config).unwrap().enrollment;
    settings.operator_config = Some(config.clone());
    (settings, config)
}

fn plain_hire(settings: &duded::Settings, cwd: &Path) -> dudes_proto::rpc::Response {
    let params = serde_json::to_value(common::hire_params(cwd)).unwrap();
    assert!(params.get("checkout").is_none());
    assert!(params.get("from").is_none());
    call(settings, Method::AgentHire, params)
}

fn hired(response: dudes_proto::rpc::Response) -> AgentId {
    assert!(response.error.is_none(), "{:?}", response.error);
    serde_json::from_value::<dudes_proto::rpc::agent::HireResult>(response.result.unwrap())
        .unwrap()
        .agent
}

#[test]
fn a_configured_legacy_hire_starts_its_first_goal_task() {
    let fixture = Fixture::new();
    let roots = tempfile::tempdir().unwrap();
    let origin = create_origin(&roots.path().canonicalize().unwrap(), "web");
    let (settings, _) = legacy_configured(&fixture, &origin.value.origin.directory);
    let host = SharedHost::new();
    let mut running = common::start(open(&settings, &host));
    let agent = hired(plain_hire(&settings, &origin.value.origin.directory));
    let mut goal = common::goal_params(agent, &[common::false_program()]);
    goal.text = "Perform the configured first task".into();
    let response = call(
        &settings,
        Method::GoalSet,
        serde_json::to_value(goal).unwrap(),
    );
    assert!(response.error.is_none(), "{:?}", response.error);
    fixture.await_record("configured first task spawn", |record| {
        matches!(record, Record::TurnSpawned { agent: owner, .. } if *owner == agent).then_some(())
    });
    assert!(fixture.records().iter().any(|record| {
        record.turn_issue().is_some_and(|issue| {
            issue.agent == agent && issue.task.contains("configured first task")
        })
    }));
    assert!(!host.spawned().is_empty());
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn an_unassigned_hire_cannot_resume_after_its_folder_becomes_an_explicit_member() {
    let fixture = Fixture::new();
    let roots = tempfile::tempdir().unwrap();
    let root = roots.path().canonicalize().unwrap();
    let origins: Vec<_> = ["web", "api", "docs"]
        .iter()
        .map(|name| create_origin(&root, name))
        .collect();
    let unrelated = create_origin(&root, "unrelated");
    let (mut settings, _) = configured(&fixture, &origins);
    settings.enrollment.profiles.clear();
    let host = SharedHost::new();
    let mut running = common::start(open(&settings, &host));
    let covered = hired(plain_hire(&settings, &origins[0].value.origin.directory));
    let outside = hired(plain_hire(&settings, &unrelated.value.origin.directory));
    let ancestor = hired(plain_hire(&settings, &root));
    for agent in [covered, outside, ancestor] {
        running.handle.ask(duded::ports::Ask::Pause(agent)).unwrap();
    }
    let checked = call(&settings, Method::FloorEnrollmentReload, json!({}))
        .result
        .unwrap();
    let applied = call(
        &settings,
        Method::FloorEnrollmentReload,
        json!({"apply": {
            "sha256": checked["sha256"], "added": ["service"],
            "removed": [], "changed": [],
        }}),
    );
    assert!(applied.error.is_none(), "{:?}", applied.error);
    assert_eq!(applied.result.unwrap()["applied"], true);
    for agent in [outside, ancestor] {
        assert!(running.handle.ask(duded::ports::Ask::Resume(agent)).is_ok());
    }
    let before = fixture.entries();
    assert!(
        plain_hire(&settings, &origins[0].value.origin.directory)
            .error
            .is_some()
    );
    assert_eq!(fixture.entries(), before);
    assert!(
        running
            .handle
            .ask(duded::ports::Ask::Resume(covered))
            .is_err(),
        "an unassigned hire resumed inside an explicit member"
    );
    assert_eq!(fixture.entries(), before);
    assert!(host.spawned().is_empty());
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    settings.enrollment = duded::config::load_operator(settings.operator_config.as_ref().unwrap())
        .unwrap()
        .enrollment;
    let daemon = open(&settings, &host);
    for agent in [covered, outside, ancestor] {
        assert!(
            daemon.world().agents[&agent]
                .repositories
                .workspace()
                .is_none()
        );
    }
    assert_eq!(
        daemon.world().agents[&covered]
            .repositories
            .singleton()
            .unwrap()
            .cwd(),
        origins[0].value.origin.directory
    );
    assert!(daemon.world().agents[&covered].paused);
    let mut running = common::start(daemon);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn a_plain_operator_hire_cannot_bypass_a_nonprimary_member_or_its_descendant() {
    let fixture = Fixture::new();
    let roots = tempfile::tempdir().unwrap();
    let root = roots.path().canonicalize().unwrap();
    let origins: Vec<_> = ["web", "api", "docs"]
        .iter()
        .map(|name| create_origin(&root, name))
        .collect();
    let (settings, _) = configured(&fixture, &origins);
    let child = origins[0].value.origin.directory.join("src");
    std::fs::create_dir(&child).unwrap();
    let host = SharedHost::new();
    let daemon = open(&settings, &host);
    assert!(daemon.world().repository_projects.is_empty());
    let mut running = common::start(daemon);
    let before = fixture.entries();
    assert!(
        call(
            &settings,
            Method::WorkspaceTrust,
            json!({"workspace_id":"service","repo_id":"web","trusted":true})
        )
        .error
        .is_some()
    );
    assert_eq!(fixture.entries(), before);
    for cwd in [
        &origins[0].value.origin.directory,
        &child,
        &origins[1].value.origin.directory,
    ] {
        let before = fixture.entries();
        let response = plain_hire(&settings, cwd);
        assert!(
            response.error.is_some(),
            "plain hire escaped the explicit project: {:?}",
            response.result
        );
        assert_eq!(fixture.entries(), before);
        assert!(host.spawned().is_empty());
    }
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

#[test]
fn an_unapplied_malformed_file_keeps_the_installed_plain_hire_authority() {
    let fixture = Fixture::new();
    let roots = tempfile::tempdir().unwrap();
    let origin = create_origin(&roots.path().canonicalize().unwrap(), "web");
    let (settings, config) = legacy_configured(&fixture, &origin.value.origin.directory);
    let host = SharedHost::new();
    let mut running = common::start(open(&settings, &host));
    std::fs::write(config, "[invalid").unwrap();
    assert!(
        call(&settings, Method::FloorEnrollmentReload, json!({}))
            .error
            .is_some()
    );
    let before = fixture.entries();
    let response = plain_hire(&settings, &origin.value.origin.directory);
    assert!(response.error.is_none(), "{:?}", response.error);
    assert!(
        fixture
            .entries()
            .iter()
            .skip(before.len())
            .all(|(_, entry)| { !matches!(entry.record, Record::Pause { .. }) })
    );
    assert!(host.spawned().is_empty());
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

fn reload_current(settings: &duded::Settings) -> dudes_proto::rpc::Response {
    let checked = call(settings, Method::FloorEnrollmentReload, json!({}));
    if let Some(result) = &checked.result {
        call(
            settings,
            Method::FloorEnrollmentReload,
            json!({"apply": {
                "sha256": result["sha256"], "added": [], "removed": [], "changed": [],
            }}),
        )
    } else {
        checked
    }
}

#[test]
fn ordinary_legacy_start_and_reload_leave_catalog_writes_dormant() {
    let fixture = Fixture::new();
    let roots = tempfile::tempdir().unwrap();
    let origin = create_origin(&roots.path().canonicalize().unwrap(), "web");
    let (settings, _) = legacy_configured(&fixture, &origin.value.origin.directory);
    let host = SharedHost::new();
    for cold in [false, false, true] {
        if cold {
            std::fs::remove_file(settings.state_dir.join(duded::boot::PROJECTION_FILE)).unwrap();
        }
        let daemon = open(&settings, &host);
        assert!(daemon.world().repository_projects.is_empty());
        let mut running = common::start(daemon);
        let response = reload_current(&settings);
        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(response.result.unwrap()["applied"], true);
        assert!(
            fixture.entries().iter().all(|(_, entry)| !matches!(
                entry.record,
                Record::ProjectRepositoriesRecorded { .. }
            ))
        );
        assert!(host.spawned().is_empty());
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

#[test]
fn an_unrecorded_current_binding_cannot_downgrade_to_a_gitfile_on_reload() {
    for remove_first in [false, true] {
        let fixture = Fixture::new();
        let roots = tempfile::tempdir().unwrap();
        let root = roots.path().canonicalize().unwrap();
        let original = create_origin(&root, "web");
        let replacement = create_origin(&root, "replacement");
        let directory = original.value.origin.directory.clone();
        let (settings, config) = legacy_configured(&fixture, &directory);
        let host = SharedHost::new();
        let daemon = open(&settings, &host);
        assert!(daemon.world().repository_projects.is_empty());
        let mut running = common::start(daemon);
        if remove_first {
            let original = std::fs::read(&config).unwrap();
            dudes_config::workspace::remove(&config, "service").unwrap();
            let checked = call(&settings, Method::FloorEnrollmentReload, json!({}))
                .result
                .unwrap();
            let applied = call(
                &settings,
                Method::FloorEnrollmentReload,
                json!({"apply": {
                    "sha256": checked["sha256"], "added": [],
                    "removed": ["service"], "changed": [],
                }}),
            );
            assert!(applied.error.is_none(), "{:?}", applied.error);
            assert_eq!(applied.result.unwrap()["applied"], true);
            std::fs::write(&config, original).unwrap();
        }
        std::fs::rename(&directory, root.join("preserved-web")).unwrap();
        git(
            &replacement.value.origin.directory,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                directory.to_str().unwrap(),
                "HEAD",
            ],
        );
        assert!(directory.join(".git").is_file());
        let before = fixture.entries();
        assert!(reload_current(&settings).error.is_some());
        assert!(plain_hire(&settings, &directory).error.is_some());
        assert_eq!(fixture.entries(), before);
        assert!(host.spawned().is_empty());
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one replacement fixture checks live refusal and both cache paths while retaining the original binding"
)]
fn a_recorded_plain_binding_cannot_downgrade_to_a_gitfile_at_the_same_path() {
    let fixture = Fixture::new();
    let roots = tempfile::tempdir().unwrap();
    let root = roots.path().canonicalize().unwrap();
    let original = create_origin(&root, "web");
    let replacement = create_origin(&root, "replacement");
    let directory = original.value.origin.directory.clone();
    let (mut settings, config) = legacy_configured(&fixture, &directory);
    let primary = RepoId::parse("primary").unwrap();
    record_catalog(
        &fixture,
        RepositorySet::new(
            primary.clone(),
            vec![RepositoryEntry {
                repo_id: primary,
                value: CatalogRepository {
                    label: "Service".into(),
                    origin: original.value.origin.clone(),
                },
            }],
        )
        .unwrap(),
    );
    let host = SharedHost::new();
    let daemon = open(&settings, &host);
    let binding = daemon.world().repository_projects["service"]
        .bindings
        .clone();
    let mut running = common::start(daemon);
    let agent = hired(plain_hire(&settings, &directory));
    running.handle.ask(duded::ports::Ask::Pause(agent)).unwrap();
    std::fs::rename(&directory, root.join("preserved-web")).unwrap();
    git(
        &replacement.value.origin.directory,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            directory.to_str().unwrap(),
            "HEAD",
        ],
    );
    assert!(directory.join(".git").is_file());
    assert_ne!(
        identity(&directory),
        original.value.origin.directory_identity
    );
    let reloaded = reload_current(&settings);
    let before_resume = fixture.entries();
    let resumed = running.handle.ask(duded::ports::Ask::Resume(agent));
    let goal = common::goal_params(agent, &[common::false_program()]);
    let _ = call(
        &settings,
        Method::GoalSet,
        serde_json::to_value(goal).unwrap(),
    );
    assert!(
        resumed.is_err(),
        "reload {:?} discarded the observed identity and resumed",
        reloaded.result
    );
    assert!(reloaded.error.is_some());
    assert!(
        fixture
            .entries()
            .iter()
            .skip(before_resume.len())
            .all(|(_, entry)| {
                !matches!(entry.record, Record::Resume { agent: who, .. } if who == agent)
                    && entry.record.turn_issue().is_none()
            })
    );
    assert!(host.spawned().is_empty());
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    settings.enrollment = duded::config::load_operator(&config).unwrap().enrollment;
    for cold in [false, true] {
        if cold {
            std::fs::remove_file(settings.state_dir.join(duded::boot::PROJECTION_FILE)).unwrap();
        }
        let daemon = open(&settings, &host);
        assert_eq!(
            daemon.world().repository_projects["service"].bindings,
            binding
        );
        let singleton = daemon.world().agents[&agent]
            .repositories
            .singleton()
            .unwrap();
        assert_eq!(singleton.cwd(), directory);
        assert_eq!(singleton.workspace().unwrap().id, "service");
        assert!(daemon.world().agents[&agent].paused);
        let mut running = common::start(daemon);
        let before = fixture.entries();
        assert!(
            running
                .handle
                .ask(duded::ports::Ask::Resume(agent))
                .is_err()
        );
        assert_eq!(fixture.entries(), before);
        assert!(host.spawned().is_empty());
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

#[test]
fn a_never_observed_legacy_gitfile_keeps_its_original_singleton_behavior() {
    let fixture = Fixture::new();
    let roots = tempfile::tempdir().unwrap();
    let root = roots.path().canonicalize().unwrap();
    let origin = create_origin(&root, "source");
    let directory = root.join("work");
    git(
        &origin.value.origin.directory,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            directory.to_str().unwrap(),
            "HEAD",
        ],
    );
    assert!(directory.join(".git").is_file());
    let (settings, _) = legacy_configured(&fixture, &directory);
    let host = SharedHost::new();
    let daemon = open(&settings, &host);
    assert!(daemon.world().repository_projects.is_empty());
    let mut running = common::start(daemon);
    let agent = hired(plain_hire(&settings, &directory));
    running.handle.ask(duded::ports::Ask::Pause(agent)).unwrap();
    running
        .handle
        .ask(duded::ports::Ask::Resume(agent))
        .unwrap();
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    for cold in [false, true] {
        if cold {
            std::fs::remove_file(settings.state_dir.join(duded::boot::PROJECTION_FILE)).unwrap();
        }
        let daemon = open(&settings, &host);
        assert!(daemon.world().repository_projects.is_empty());
        let singleton = daemon.world().agents[&agent]
            .repositories
            .singleton()
            .unwrap();
        assert_eq!(singleton.cwd(), directory);
        assert_eq!(singleton.workspace().unwrap().id, "service");
        let mut running = common::start(daemon);
        running
            .handle
            .ask(duded::ports::Ask::Resume(agent))
            .unwrap();
        assert!(host.spawned().is_empty());
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

#[test]
fn a_missing_current_project_cannot_release_live_recorded_authority_or_admit_a_plain_hire() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let settings = settings(&fixture);
    assert!(settings.enrollment.profiles.is_empty());
    let origin = story.hire.repositories.repositories.entries()[0]
        .value
        .origin
        .directory
        .clone();
    let host = SharedHost::new();
    for cold in [false, false, true] {
        if cold {
            std::fs::remove_file(settings.state_dir.join(duded::boot::PROJECTION_FILE)).unwrap();
        }
        let daemon = open(&settings, &host);
        assert!(!daemon.world().agents[&story.hire.id].paused);
        assert_eq!(
            scoped_hire(daemon.world(), story.hire.id).recorded(),
            story.hire.repositories
        );
        let mut running = common::start(daemon);
        let before = fixture.entries();
        let response = plain_hire(&settings, &origin);
        assert!(
            response.error.is_some(),
            "missing current project admitted a plain hire: {:?}",
            response.result
        );
        assert!(
            running
                .handle
                .ask(duded::ports::Ask::Resume(story.hire.id))
                .is_err()
        );
        assert_eq!(fixture.entries(), before);
        assert!(host.spawned().is_empty());
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}

#[test]
fn a_missing_current_project_cannot_forget_an_unresolved_declaration_after_retirement() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let member = &story.hire.repositories.repositories.entries()[0];
    let Record::TicketCreatedWithRepositories(mut ticket) =
        samples::record(RecordKind::TicketCreatedWithRepositories)
    else {
        unreachable!()
    };
    ticket.declaration = dudes_proto::repository::TicketDeclaration {
        project: "service".into(),
        catalog_lsn: story.hire.repositories.catalog_lsn,
        repositories: RepositorySet::new(
            member.repo_id.clone(),
            vec![RepositoryEntry {
                repo_id: member.repo_id.clone(),
                value: member.value.origin.clone(),
            }],
        )
        .unwrap(),
    };
    ticket.check_repo = member.repo_id.clone();
    ticket.request = None;
    let settings = settings(&fixture);
    let (mut writer, _) = dudes_journal::open(&fixture.journal_dir(), settings.journal).unwrap();
    append(&mut writer, Record::TicketCreatedWithRepositories(ticket));
    append(
        &mut writer,
        Record::AgentRetired {
            id: story.hire.id,
            reason: "Completed previous work".into(),
            browser_retire: None,
        },
    );
    drop(writer);
    let host = SharedHost::new();
    for cold in [false, true] {
        if cold {
            std::fs::remove_file(settings.state_dir.join(duded::boot::PROJECTION_FILE)).unwrap();
        }
        let daemon = open(&settings, &host);
        assert!(daemon.world().agents[&story.hire.id].retired);
        let mut running = common::start(daemon);
        let before = fixture.entries();
        let response = plain_hire(&settings, &member.value.origin.directory);
        assert!(
            response.error.is_some(),
            "missing unresolved project admitted a plain hire: {:?}",
            response.result
        );
        assert_eq!(fixture.entries(), before);
        assert!(host.spawned().is_empty());
        assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
    }
}
