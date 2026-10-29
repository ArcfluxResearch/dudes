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

struct Active {
    running: common::Running,
    fixture: Fixture,
    host: SharedHost,
    cli: Cli,
    config: std::path::PathBuf,
    agent: AgentId,
    turn: dudes_types::TurnId,
}

impl Active {
    fn new() -> Self {
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
        let running = common::start(&settings, host.clone(), Arc::new(OpenRunner::default()));
        let cli = Cli::new(&settings.socket(), &settings.runtime_dir)
            .env("HOME", &home)
            .env("DUDES_CONFIG", &config)
            .current_dir(&a);
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
        Self {
            running,
            fixture,
            host,
            cli,
            config,
            agent,
            turn,
        }
    }

    fn unchanged(&self, before: &[(dudes_types::Lsn, dudes_proto::Entry)]) {
        assert!(
            self.fixture
                .records()
                .iter()
                .all(|record| !matches!(record, Record::Pause { .. })),
            "a read appended a Pause record"
        );
        assert_eq!(self.fixture.entries(), before, "a read wrote the journal");
        assert_running(&self.cli, self.agent);
        assert!(self.fixture.records().iter().all(|record| !matches!(record,
            Record::TurnEnded { turn, .. } if *turn == self.turn)));
        assert_eq!(self.host.children(), 1);
    }
}

#[test]
fn a_reload_preview_for_another_project_keeps_an_owned_turn_running_without_writes() {
    let active = Active::new();
    let before = active.fixture.entries();
    dudes_config::workspace::remove(&active.config, "b").unwrap();
    let preview = active
        .cli
        .clone()
        .args(["--json", "project", "reload"])
        .run();
    assert_eq!(preview.code(), exit::SUCCESS, "{}", preview.output());
    let preview: EnrollmentReloadResult = preview.json();
    assert!(!preview.applied);
    assert_eq!(preview.removed, ["b"]);
    active.unchanged(&before);
    for args in [vec!["status"], vec!["start"], vec!["project", "list"]] {
        let read = active.cli.clone().args(args).run();
        assert_eq!(read.code(), exit::SUCCESS, "{}", read.output());
        active.unchanged(&before);
    }
}

#[test]
fn a_refused_rebind_and_implicit_hire_leave_the_installed_projects_and_running_turn_alone() {
    let active = Active::new();
    let before = active.fixture.entries();
    let original = std::fs::read_to_string(&active.config).unwrap();
    let b = active.config.parent().unwrap().join("b");
    let b = b.canonicalize().unwrap();
    let other = active.config.parent().unwrap().join("replacement");
    std::fs::create_dir_all(other.join(".git")).unwrap();
    let other = other.canonicalize().unwrap();
    std::fs::write(
        &active.config,
        original.replace(b.to_str().unwrap(), other.to_str().unwrap()),
    )
    .unwrap();
    let preview = active.cli.clone().args(["project", "reload"]).run();
    assert_eq!(preview.code(), exit::REFUSED, "{}", preview.output());
    assert!(
        preview
            .stderr
            .contains("an observed repository id cannot be rebound"),
        "{}",
        preview.output()
    );
    active.unchanged(&before);
    let hire = active
        .cli
        .clone()
        .args([
            "hire",
            "--name",
            "Other",
            "--role",
            "infra",
            "--department",
            "infra",
            "--provider",
            "claude",
            "--usd-per-day",
            "10",
        ])
        .run();
    assert_eq!(hire.code(), exit::REFUSED, "{}", hire.output());
    assert!(
        hire.stderr
            .contains("an observed repository id cannot be rebound"),
        "{}",
        hire.output()
    );
    active.unchanged(&before);
    std::fs::write(&active.config, "[invalid").unwrap();
    let malformed = active.cli.clone().args(["project", "reload"]).run();
    assert_eq!(malformed.code(), exit::OTHER, "{}", malformed.output());
    active.unchanged(&before);
    std::fs::write(&active.config, original).unwrap();
    let preview = active.cli.clone().args(["project", "reload"]).run();
    assert_eq!(preview.code(), exit::SUCCESS, "{}", preview.output());
    active.unchanged(&before);
}

#[test]
fn a_missing_project_holds_only_its_next_turn_and_recovers_without_resume() {
    let active = Active::new();
    let b = active.config.parent().unwrap().join("b");
    let b = b.canonicalize().unwrap();
    let hired = active
        .cli
        .clone()
        .arg("--json")
        .args(common::hire_args(&b))
        .run();
    assert_eq!(hired.code(), exit::SUCCESS, "{}", hired.output());
    let agent = hired.json::<HireResult>().agent;
    let goal = active
        .cli
        .clone()
        .args(common::goal_args(&agent.to_string(), "keep working in b"))
        .run();
    assert_eq!(goal.code(), exit::SUCCESS, "{}", goal.output());
    let (turn, key) = active
        .fixture
        .await_record("B's owned turn", |record| match record {
            Record::TurnSpawned {
                agent: owner,
                turn,
                pid,
                start_time,
                boot,
                ..
            } if *owner == agent => Some((
                *turn,
                dudes_supervisor::AdoptionKey {
                    pid: *pid,
                    start_time: *start_time,
                    boot: boot.clone(),
                },
            )),
            _ => None,
        });
    let missing = b.with_file_name("preserved-b");
    std::fs::rename(&b, &missing).unwrap();
    active.running.handle.input(dudes_supervisor::Input::Tick);
    assert_running(&active.cli, active.agent);
    active.host.mark_native_reaped(&key);
    active.host.finish_owned_child(
        &key,
        dudes_supervisor::Input::ChildExited {
            turn,
            exit: dudes_proto::ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: None,
            model_usage: std::collections::BTreeMap::new(),
            report: dudes_proto::TurnReport::default(),
            final_reply: None,
            elapsed_ms: None,
            outcome: dudes_proto::TurnOutcome::Completed,
            last_message: None,
        },
    );
    active
        .fixture
        .await_record("B's completed turn while held", |record| {
            matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn).then_some(())
        });
    let issues = || {
        active
            .fixture
            .records()
            .iter()
            .filter_map(Record::turn_issue)
            .filter(|issue| issue.agent == agent)
            .count()
    };
    assert_eq!(issues(), 1);
    assert_running(&active.cli, active.agent);
    std::fs::rename(missing, &b).unwrap();
    active.running.handle.input(dudes_supervisor::Input::Tick);
    active
        .fixture
        .await_record("B's recovered next turn", |record| match record {
            Record::TurnSpawned {
                agent: owner,
                turn: next,
                ..
            } if *owner == agent && *next != turn => Some(()),
            _ => None,
        });
    assert_eq!(issues(), 2);
    assert_running(&active.cli, active.agent);
    assert!(
        active
            .fixture
            .records()
            .iter()
            .all(|record| !matches!(record, Record::Pause { .. } | Record::Resume { .. }))
    );
}
