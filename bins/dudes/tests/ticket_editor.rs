//! Editors receive the ticket's exact recorded directory through the real CLI.
#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Cli, Fixture, OpenRunner, Running, SharedHost, start};
use duded::floor_actions::{EnrolledCheck, Enrollment, Profile};
use dudes_proto::rpc::agent::HireResult;
use dudes_proto::rpc::ticket::TicketCreateResult;
use dudes_proto::{AgentBudget, GoalBudget, Predicate};
use dudes_types::{IsolationMode, Usd};

struct Review {
    _running: Running,
    fixture: Fixture,
    cli: Cli,
    directory: PathBuf,
    card: String,
}

fn review() -> Review {
    let fixture = Fixture::new();
    let repo = fixture.state.path().join("repository");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(repo.join("review files")).unwrap();
    let repo = repo.canonicalize().unwrap();
    let directory = repo.join("review files");
    let mut settings = fixture.settings();
    settings.enrollment = Enrollment {
        profiles: vec![Profile {
            id: "service".to_owned(),
            label: "Service".to_owned(),
            checks: vec![EnrolledCheck {
                id: "default".to_owned(),
                label: "Checks pass".to_owned(),
                predicate: Predicate::Command {
                    argv: vec!["/usr/bin/true".to_owned()],
                    timeout_s: 10,
                },
                may_already_pass: true,
            }],
            cwd: directory.clone(),
            written_cwd: directory.clone(),
            department: "engineering".to_owned(),
            isolation: IsolationMode::Default,
            agent_budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: None,
            },
            goal_budget: GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
            hop_cap: 4,
            mark: None,
        }],
    };
    let _running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir);
    let hired = cli
        .clone()
        .args([
            "--json",
            "hire",
            "--name",
            "Cody",
            "--role",
            "engineer",
            "--department",
            "engineering",
            "--provider",
            "claude",
            "--cwd",
        ])
        .arg(&directory)
        .args(["--usd-per-day", "10"])
        .run();
    assert_eq!(hired.code(), 0, "{}", hired.output());
    let agent = hired.json::<HireResult>().agent.to_string();
    let paused = cli.clone().args(["pause", &agent]).run();
    assert_eq!(paused.code(), 0, "{}", paused.output());
    let added = cli
        .clone()
        .args([
            "--json",
            "ticket",
            "add",
            "--title",
            "Review locally",
            "--project",
            "service",
        ])
        .run();
    assert_eq!(added.code(), 0, "{}", added.output());
    let card = added.json::<TicketCreateResult>().card.to_string();
    let assigned = cli
        .clone()
        .args(["ticket", "give", &card, "--agent", &agent])
        .run();
    assert_eq!(assigned.code(), 0, "{}", assigned.output());

    Review {
        _running,
        fixture,
        cli,
        directory,
        card,
    }
}

fn recorder(review: &Review, name: &str, source: &[u8]) -> PathBuf {
    let editor = review.fixture.runtime.path().join(name);
    fs::write(&editor, source).unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    editor
}

#[test]
fn an_explicit_editor_receives_only_the_recorded_directory_and_owns_the_exit_code() {
    let review = review();
    let editor = recorder(
        &review,
        "recording editor",
        b"#!/bin/sh\nprintf 'argc=%s\\noperand=%s\\nsocket=%s\\n' \"$#\" \"$1\" \"${DUDES_SOCK-unset}\"\nexit 37\n",
    );
    let opened = review
        .cli
        .args(["ticket", "open", &review.card, "--editor"])
        .arg(&editor)
        .run();

    assert_eq!(opened.code(), 37, "{}", opened.output());
    assert_eq!(
        opened.stdout,
        format!(
            "argc=1\noperand={}\nsocket=unset\n",
            review.directory.display()
        )
    );
    assert_eq!(opened.stderr, "");
}

#[test]
fn visual_takes_precedence_and_passes_its_arguments_before_the_recorded_directory() {
    let review = review();
    let visual = recorder(
        &review,
        "visual-editor",
        b"#!/bin/sh\nprintf 'argc=%s\\n' \"$#\"\nprintf '%s\\n' \"$@\"\nexit 37\n",
    );
    let editor = recorder(
        &review,
        "fallback-editor",
        b"#!/bin/sh\nprintf 'EDITOR ran\\n'\nexit 83\n",
    );
    let opened = review
        .cli
        .env(
            "VISUAL",
            format!("  {} \t --reuse-window   review-mode  ", visual.display()),
        )
        .env("EDITOR", editor)
        .args(["ticket", "open", &review.card])
        .run();

    assert_eq!(opened.code(), 37, "{}", opened.output());
    assert_eq!(
        opened.stdout,
        format!(
            "argc=3\n--reuse-window\nreview-mode\n{}\n",
            review.directory.display()
        )
    );
    assert_eq!(opened.stderr, "");
}

#[test]
fn a_bare_editor_is_found_on_the_operators_path_with_its_arguments() {
    let review = review();
    let editor = recorder(
        &review,
        "path-editor",
        b"#!/bin/sh\nprintf 'argc=%s\\n' \"$#\"\nprintf '%s\\n' \"$@\"\nexit 37\n",
    );
    let path = std::env::join_paths([
        editor.parent().unwrap(),
        Path::new("/usr/bin"),
        Path::new("/bin"),
    ])
    .unwrap();
    let opened = review
        .cli
        .env("PATH", path)
        .env("VISUAL", "path-editor --reuse-window review-mode")
        .args(["ticket", "open", &review.card])
        .run();

    assert_eq!(opened.code(), 37, "{}", opened.output());
    assert_eq!(
        opened.stdout,
        format!(
            "argc=3\n--reuse-window\nreview-mode\n{}\n",
            review.directory.display()
        )
    );
    assert_eq!(opened.stderr, "");
}
