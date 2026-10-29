//! Reading an edited operator file leaves the running enrollment alone.
#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;

use dudes::exit;
use dudes_config::workspace::NewWorkspace;
use dudes_proto::rpc::agent::HireResult;
use dudes_proto::rpc::floor::EnrollmentReloadResult;
use dudes_proto::{AgentView, Record};
use dudes_types::{AgentId, AgentStatus};

mod common;
use common::{Cli, Fixture, OpenRunner, SharedHost};

fn add(config: &Path, root: &Path, id: &str) -> std::path::PathBuf {
    let directory = root.join(id);
    std::fs::create_dir_all(directory.join(".git")).unwrap();
    let directory = directory.canonicalize().unwrap();
    dudes_config::workspace::add(
        config,
        &NewWorkspace::with_defaults(
            directory.clone(),
            id.into(),
            id.into(),
            dudes_config::check::changed_check(),
        ),
    )
    .unwrap();
    directory
}

fn assert_running(cli: &Cli, agent: AgentId) {
    let result = cli.clone().args(["--json", "agents"]).run();
    assert_eq!(result.code(), exit::SUCCESS, "{}", result.output());
    let agents: Vec<AgentView> = result.json();
    let row = agents.iter().find(|row| row.id == agent).unwrap();
    assert_eq!(row.status, AgentStatus::Running);
    assert!(!row.paused);
}

#[test]
fn a_reload_preview_for_another_project_keeps_an_owned_turn_running_without_writes() {
    let fixture = Fixture::new();
    let home = fixture.state.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let config = home.join("operator.toml");
    let a = add(&config, &home, "a");
    add(&config, &home, "b");
    let mut settings = fixture.settings();
    settings.enrollment = duded::config::load_operator(&config).unwrap().enrollment;
    settings.operator_config = Some(config.clone());
    let host =
        SharedHost::with_native_evidence(fixture.state.path().join(duded::boot::JOURNAL_DIR));
    let _running = common::start(&settings, host.clone(), Arc::new(OpenRunner::default()));
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir)
        .env("HOME", &home)
        .env("DUDES_CONFIG", &config);
    let hired = cli.clone().arg("--json").args(common::hire_args(&a)).run();
    assert_eq!(hired.code(), exit::SUCCESS, "{}", hired.output());
    let agent = hired.json::<HireResult>().agent;
    let goal = cli
        .clone()
        .args(common::goal_args(&agent.to_string(), "keep working in a"))
        .run();
    assert_eq!(goal.code(), exit::SUCCESS, "{}", goal.output());
    let turn = fixture.await_record("owned running turn", |record| match record {
        Record::TurnSpawned {
            agent: owner, turn, ..
        } if *owner == agent => Some(*turn),
        _ => None,
    });
    assert_eq!(host.children(), 1);
    assert_running(&cli, agent);
    let before = fixture.entries();
    dudes_config::workspace::remove(&config, "b").unwrap();
    let preview = cli.clone().args(["--json", "project", "reload"]).run();
    assert_eq!(preview.code(), exit::SUCCESS, "{}", preview.output());
    let preview: EnrollmentReloadResult = preview.json();
    assert!(!preview.applied);
    assert_eq!(preview.removed, ["b"]);
    assert!(
        fixture
            .records()
            .iter()
            .all(|record| !matches!(record, Record::Pause { .. })),
        "a preview appended a Pause record"
    );
    assert_eq!(fixture.entries(), before, "a preview wrote the journal");
    assert_running(&cli, agent);
    assert!(fixture.records().iter().all(|record| !matches!(record,
        Record::TurnEnded { turn: ended, .. } if *ended == turn)));
    assert_eq!(host.children(), 1);
}
