//! Hire paths identify one canonical directory in the daemon's recorded state.
#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dudes::exit;
use dudes_proto::record::Record;

mod common;

use common::{Cli, Fixture, OpenRunner, SharedHost, start};

fn hire_in(cli: &Cli, here: &Path, cwd: &str) -> common::Run {
    hire_path(cli, here, Path::new(cwd))
}

fn hire_path(cli: &Cli, here: &Path, cwd: &Path) -> common::Run {
    let mut args = common::hire_args(cwd);
    args.insert(0, OsString::from("--json"));
    cli.clone().current_dir(here).args(args).run()
}

fn canonical_hire(typed: impl FnOnce(&Path) -> PathBuf) {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir);
    let root = tempfile::tempdir().expect("path fixture");
    let repo = root.path().join("repository with spaces");
    fs::create_dir_all(repo.join("nested")).expect("repository");
    symlink(&repo, root.path().join("alias")).expect("repository alias");
    let canonical = repo.canonicalize().expect("canonical directory");
    let result = hire_path(&cli, root.path(), &typed(root.path()));
    assert_eq!(result.code(), exit::SUCCESS, "{}", result.output());
    let directories = hired_directories(&fixture);
    assert_eq!(directories.len(), 1);
    assert_eq!(directories[0].as_os_str(), canonical.as_os_str());
}

#[test]
fn a_canonical_absolute_cwd_is_recorded_unchanged() {
    canonical_hire(|root| root.join("repository with spaces").canonicalize().unwrap());
}

#[test]
fn an_absolute_symlink_cwd_is_recorded_canonically() {
    canonical_hire(|root| root.join("alias"));
}

#[test]
fn an_absolute_dot_cwd_is_recorded_canonically() {
    canonical_hire(|root| root.join("repository with spaces").join("."));
}

#[test]
fn an_absolute_parent_cwd_is_recorded_canonically() {
    canonical_hire(|root| root.join("alias").join("nested").join(".."));
}

#[test]
fn a_relative_dot_from_a_symlinked_directory_records_its_canonical_target() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let alias = root.path().join("alias");
    symlink(&repo, &alias).unwrap();
    let result = hire_in(
        &Cli::new(&settings.socket(), &settings.runtime_dir),
        &alias,
        ".",
    );
    assert_eq!(result.code(), exit::SUCCESS, "{}", result.output());
    assert_eq!(hired_directories(&fixture), [repo.canonicalize().unwrap()]);
}

#[test]
fn a_relative_parent_cwd_records_the_canonical_parent() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let result = hire_in(
        &Cli::new(&settings.socket(), &settings.runtime_dir),
        &nested,
        "..",
    );
    assert_eq!(result.code(), exit::SUCCESS, "{}", result.output());
    assert_eq!(
        hired_directories(&fixture),
        [root.path().canonicalize().unwrap()]
    );
}

fn refused_directory(absolute: bool, file: bool) {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let root = tempfile::tempdir().unwrap();
    if file {
        fs::write(root.path().join("input"), b"a file").unwrap();
    }
    let typed = if absolute {
        root.path().join("input")
    } else {
        PathBuf::from("input")
    };
    let result = hire_path(
        &Cli::new(&settings.socket(), &settings.runtime_dir),
        root.path(),
        &typed,
    );
    assert_eq!(result.code(), exit::OTHER, "{}", result.output());
    assert!(
        hired_directories(&fixture).is_empty(),
        "a rejected path records no hire"
    );
}

#[test]
fn a_missing_absolute_cwd_is_rejected_without_hiring() {
    refused_directory(true, false);
}

#[test]
fn an_absolute_regular_file_is_rejected_without_hiring() {
    refused_directory(true, true);
}

#[test]
fn a_missing_relative_cwd_is_rejected_without_hiring() {
    refused_directory(false, false);
}

#[test]
fn a_relative_regular_file_is_rejected_without_hiring() {
    refused_directory(false, true);
}

#[test]
fn an_empty_cwd_is_rejected_without_hiring() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let result = hire_in(
        &Cli::new(&settings.socket(), &settings.runtime_dir),
        fixture.state.path(),
        "",
    );
    assert_eq!(result.code(), exit::OTHER, "{}", result.output());
    assert!(hired_directories(&fixture).is_empty());
}

fn absolute_without_captured_directory(alias: bool) {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir(&repo).unwrap();
    symlink(&repo, root.path().join("alias")).unwrap();
    let canonical = repo.canonicalize().unwrap();
    let typed = if alias {
        root.path().join("alias")
    } else {
        canonical.clone()
    };
    let env = dudes::Environment::resolve(|name| match name {
        "DUDES_SOCK" => Some(settings.socket().into_os_string()),
        "DUDES_RUNTIME_DIR" => Some(settings.runtime_dir.as_os_str().to_owned()),
        "DUDES_STATE_DIR" => Some(settings.state_dir.as_os_str().to_owned()),
        _ => None,
    });
    assert!(env.current_dir().is_none());
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = dudes::run(&common::hire_args(&typed), &env, None, &mut out, &mut err);
    assert_eq!(code, exit::SUCCESS, "{}", String::from_utf8_lossy(&err));
    let directories = hired_directories(&fixture);
    assert_eq!(directories.len(), 1);
    assert_eq!(directories[0].as_os_str(), canonical.as_os_str());
}

fn enrolled_settings(fixture: &Fixture, repo: &Path) -> duded::Settings {
    fs::create_dir_all(repo.join(".git")).expect("repository marker");
    let config = fixture.state.path().join("operator.toml");
    fs::write(
        &config,
        format!(
            "[[floor.onboarding.workspaces]]\nworkspace_id = \"canonical_repo\"\nlabel = \"Canonical repository\"\ncompletion_check_label = \"Checks pass\"\ncwd = {}\ndepartment = \"engineering\"\nisolation = \"default\"\nagent_usd_per_day = 10\ngoal_usd = 5\nhop_cap = 3\npredicate_argv = [\"/usr/bin/true\"]\npredicate_timeout_s = 10\n",
            serde_json::to_string(repo.canonicalize().unwrap().to_str().unwrap()).unwrap()
        ),
    )
    .expect("scratch enrollment");
    let mut settings = fixture.settings();
    settings.enrollment = duded::config::load_operator(&config)
        .expect("valid enrollment")
        .enrollment;
    settings
}

fn recorded_workspace(alias: bool) {
    let fixture = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let canonical = repo.canonicalize().unwrap();
    symlink(&repo, root.path().join("alias")).unwrap();
    let settings = enrolled_settings(&fixture, &repo);
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let typed = if alias {
        root.path().join("alias")
    } else {
        canonical
    };
    let result = hire_path(
        &Cli::new(&settings.socket(), &settings.runtime_dir),
        root.path(),
        &typed,
    );
    assert_eq!(result.code(), exit::SUCCESS, "{}", result.output());
    let workspace = fixture
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::AgentHired { workspace, .. } => Some(workspace),
            _ => None,
        })
        .expect("actual hire record");
    assert_eq!(
        workspace,
        Some(dudes_proto::record::RecordedWorkspace {
            id: "canonical_repo".to_owned(),
            label: "Canonical repository".to_owned(),
        })
    );
}

#[test]
fn a_canonical_absolute_hire_records_the_enrolled_workspace() {
    recorded_workspace(false);
}

#[test]
fn an_absolute_alias_records_the_same_enrolled_workspace() {
    recorded_workspace(true);
}

#[test]
fn hire_defaults_to_the_only_enrolled_project() {
    let fixture = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let here = root.path().join("repository");
    fs::create_dir(&here).unwrap();
    let mut settings = enrolled_settings(&fixture, &here);
    let config = fixture.state.path().join("operator.toml");
    settings.operator_config = Some(config.clone());
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir)
        .env("DUDES_CONFIG", config)
        .current_dir(root.path());
    let hired = cli
        .clone()
        .args([
            "hire",
            "--name",
            "Kim",
            "--role",
            "engineer",
            "--department",
            "engineering",
            "--provider",
            "claude",
        ])
        .run();
    assert_eq!(hired.code(), exit::SUCCESS, "{}", hired.output());
    let listed = cli.args(["--json", "agents"]).run();
    let agents: Vec<dudes_proto::AgentView> = listed.json();
    assert_eq!(agents[0].workspace.as_ref().unwrap().id, "canonical_repo");
    assert_eq!(hired_directories(&fixture), [here.canonicalize().unwrap()]);
    assert!(!hired.output().contains("can't take tickets"));
}

#[test]
fn hire_without_a_project_warns_that_it_cannot_take_tickets() {
    let fixture = Fixture::new();
    let config = fixture.state.path().join("operator.toml");
    fs::write(&config, "").unwrap();
    let mut settings = fixture.settings();
    settings.operator_config = Some(config.clone());
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let root = tempfile::tempdir().unwrap();
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir)
        .env("DUDES_CONFIG", config)
        .current_dir(root.path());
    let hired = cli.clone().args(common::hire_args(root.path())).run();
    assert_eq!(hired.code(), exit::SUCCESS, "{}", hired.output());
    assert!(
        hired
            .stderr
            .contains("has no project and can't take tickets."),
        "{}",
        hired.output()
    );
    assert!(
        hired
            .stderr
            .contains("No project yet. In your project's folder run `dudes project add`."),
        "{}",
        hired.output()
    );
    let json = cli.arg("--json").args(common::hire_args(root.path())).run();
    assert_eq!(json.code(), exit::SUCCESS, "{}", json.output());
    let _: dudes_proto::rpc::agent::HireResult = json.json();
    assert!(!json.output().contains("No project yet."));
}

#[test]
fn hire_without_cwd_or_a_project_uses_here_and_warns() {
    let fixture = Fixture::new();
    let config = fixture.state.path().join("operator.toml");
    fs::write(&config, "").unwrap();
    let mut settings = fixture.settings();
    settings.operator_config = Some(config.clone());
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let root = tempfile::tempdir().unwrap();
    let hired = Cli::new(&settings.socket(), &settings.runtime_dir)
        .env("DUDES_CONFIG", config)
        .current_dir(root.path())
        .args([
            "hire",
            "--name",
            "Kim",
            "--role",
            "engineer",
            "--department",
            "engineering",
            "--provider",
            "claude",
        ])
        .run();
    assert_eq!(hired.code(), exit::SUCCESS, "{}", hired.output());
    assert_eq!(
        hired_directories(&fixture),
        [root.path().canonicalize().unwrap()]
    );
    assert!(
        hired
            .stderr
            .contains("Kim has no project and can't take tickets.")
    );
}

#[test]
fn a_hire_outside_a_project_does_not_claim_that_none_are_enrolled() {
    let fixture = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let here = root.path().join("repository");
    fs::create_dir(&here).unwrap();
    let mut settings = enrolled_settings(&fixture, &here);
    let config = fixture.state.path().join("operator.toml");
    settings.operator_config = Some(config.clone());
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir).env("DUDES_CONFIG", config);
    let hired = cli.clone().args(common::hire_args(root.path())).run();
    assert_eq!(hired.code(), exit::SUCCESS, "{}", hired.output());
    assert!(
        hired
            .stderr
            .contains("has no project and can't take tickets."),
        "{}",
        hired.output()
    );
    assert!(
        !hired.stderr.contains("No project yet."),
        "{}",
        hired.output()
    );
    assert!(
        hired
            .stderr
            .contains("Hire a new agent in a project with `dudes hire` to take tickets."),
        "{}",
        hired.output()
    );
    let listed = cli.args(["--json", "agents"]).run();
    let agents: Vec<dudes_proto::AgentView> = listed.json();
    assert!(agents[0].workspace.is_none());
}

fn sharing_warning(alias: bool) {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir(&repo).unwrap();
    symlink(&repo, root.path().join("alias")).unwrap();
    let canonical = repo.canonicalize().unwrap();
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir);
    let first = hire_path(&cli, root.path(), &canonical);
    assert_eq!(first.code(), exit::SUCCESS, "{}", first.output());
    let typed = if alias {
        root.path().join("alias")
    } else {
        canonical
    };
    let second = hire_path(&cli, root.path(), &typed);
    assert_eq!(second.code(), exit::SUCCESS, "{}", second.output());
    assert_eq!(
        hired_directories(&fixture).len(),
        2,
        "ordinary sharing stays admitted"
    );
    assert!(
        second
            .stderr
            .contains("another live agent works in this directory"),
        "{}",
        second.output()
    );
    let listed = cli.arg("--json").arg("agents").run();
    assert_eq!(listed.code(), exit::SUCCESS, "{}", listed.output());
    let agents: Vec<dudes_proto::AgentView> = listed.json();
    assert!(agents.iter().all(|agent| agent.shares_checkout));
}

#[test]
fn two_canonical_hires_keep_the_existing_sharing_warning() {
    sharing_warning(false);
}

#[test]
fn an_absolute_alias_keeps_the_existing_sharing_warning() {
    sharing_warning(true);
}

#[test]
fn a_canonical_absolute_cwd_needs_no_captured_current_directory() {
    absolute_without_captured_directory(false);
}

#[test]
fn an_absolute_symlink_is_canonicalized_without_a_captured_current_directory() {
    absolute_without_captured_directory(true);
}

fn hired_directories(fixture: &Fixture) -> Vec<PathBuf> {
    fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::AgentHired { cwd, .. } => Some(cwd),
            _ => None,
        })
        .collect()
}

#[test]
fn a_relative_cwd_is_the_operators_directory_made_absolute() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir);
    let repo = tempfile::tempdir().expect("the operator's repository");
    let canonical = repo.path().canonicalize().expect("a canonical repository");

    let here = hire_in(&cli, repo.path(), ".");
    assert_eq!(here.code(), exit::SUCCESS, "{}", here.output());
    fixture.await_record("AgentHired", |record| {
        matches!(record, Record::AgentHired { .. }).then_some(())
    });
    assert_eq!(
        hired_directories(&fixture),
        std::slice::from_ref(&canonical)
    );

    let missing = hire_in(&cli, repo.path(), "no-such-dir");
    assert_eq!(missing.code(), exit::OTHER, "{}", missing.output());
    assert_eq!(
        missing.stderr,
        format!(
            "dudes: {} does not exist\n",
            canonical.join("no-such-dir").display()
        )
    );
    assert_eq!(hired_directories(&fixture), [canonical]);
}
