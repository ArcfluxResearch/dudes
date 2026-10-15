//! Editors receive the ticket's exact recorded directory through the real CLI.
#![cfg(unix)]

mod common;

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Cli, Fixture, OpenRunner, Running, SharedHost, start};
use duded::floor_actions::{EnrolledCheck, Enrollment, Profile};
use dudes_proto::rpc::agent::HireResult;
use dudes_proto::rpc::ticket::{TicketCheckoutResult, TicketCreateResult, TicketReadResult};
use dudes_proto::{AgentBudget, GoalBudget, Predicate, Record};
use dudes_types::{IsolationMode, Usd};

struct Review {
    running: Running,
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
    let running = start(
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
        running,
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

#[test]
fn a_relative_path_entry_finds_an_editor_from_the_operator_directory() {
    let review = review();
    let editor = recorder(
        &review,
        "relative-editor",
        b"#!/bin/sh\nprintf 'argc=%s\\n' \"$#\"\nprintf '%s\\n' \"$@\"\nexit 37\n",
    );
    let path =
        std::env::join_paths([Path::new("."), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let opened = review
        .cli
        .current_dir(editor.parent().unwrap())
        .env("PATH", path)
        .env("VISUAL", "relative-editor")
        .args(["ticket", "open", &review.card])
        .run();

    assert_eq!(opened.code(), 37, "{}", opened.output());
    assert_eq!(
        opened.stdout,
        format!("argc=1\n{}\n", review.directory.display())
    );
    assert_eq!(opened.stderr, "");
}

#[test]
fn an_empty_path_entry_finds_an_editor_from_the_operator_directory() {
    let review = review();
    let editor = recorder(
        &review,
        "empty-editor",
        b"#!/bin/sh\nprintf 'argc=%s\\n' \"$#\"\nprintf '%s\\n' \"$@\"\nexit 37\n",
    );
    let path =
        std::env::join_paths([Path::new(""), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let opened = review
        .cli
        .current_dir(editor.parent().unwrap())
        .env("PATH", path)
        .env("VISUAL", "empty-editor")
        .args(["ticket", "open", &review.card])
        .run();

    assert_eq!(opened.code(), 37, "{}", opened.output());
    assert_eq!(
        opened.stdout,
        format!("argc=1\n{}\n", review.directory.display())
    );
    assert_eq!(opened.stderr, "");
}

#[test]
fn the_first_executable_inside_the_checkout_refuses_even_with_a_later_safe_candidate() {
    let review = review();
    let outside = recorder(
        &review,
        "selected-editor",
        b"#!/bin/sh\nprintf 'safe editor ran\\n'\nexit 37\n",
    );
    let inside = review.directory.join("selected-editor");
    fs::write(
        &inside,
        b"#!/bin/sh\nprintf 'inside editor ran\\n'\nexit 83\n",
    )
    .unwrap();
    fs::set_permissions(&inside, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths([
        Path::new(""),
        outside.parent().unwrap(),
        Path::new("/usr/bin"),
        Path::new("/bin"),
    ])
    .unwrap();
    let opened = review
        .cli
        .current_dir(&review.directory)
        .env("PATH", path)
        .env("VISUAL", "selected-editor")
        .args(["ticket", "open", &review.card])
        .run();

    assert_eq!(opened.code(), 4, "{}", opened.output());
    assert_eq!(opened.stdout, "");
    assert_eq!(
        opened.stderr,
        "dudes: The editor must be outside the recorded checkout.\n"
    );
}

#[test]
fn a_blank_visual_falls_through_to_editor() {
    let review = review();
    let editor = recorder(
        &review,
        "blank-visual-editor",
        b"#!/bin/sh\nprintf 'argc=%s\\n' \"$#\"\nprintf '%s\\n' \"$@\"\nexit 37\n",
    );
    let opened = review
        .cli
        .env("VISUAL", " \t\n ")
        .env("EDITOR", format!("{} --reuse-window", editor.display()))
        .args(["ticket", "open", &review.card])
        .run();

    assert_eq!(opened.code(), 37, "{}", opened.output());
    assert_eq!(
        opened.stdout,
        format!("argc=2\n--reuse-window\n{}\n", review.directory.display())
    );
    assert_eq!(opened.stderr, "");
}

#[test]
fn an_invalid_selected_visual_refuses_without_switching_to_editor() {
    let review = review();
    let fallback = recorder(
        &review,
        "valid-fallback",
        b"#!/bin/sh\nprintf 'fallback ran\\n'\nexit 37\n",
    );
    let non_executable = review.fixture.runtime.path().join("not-executable");
    fs::write(&non_executable, b"#!/bin/sh\nexit 37\n").unwrap();
    fs::set_permissions(&non_executable, fs::Permissions::from_mode(0o644)).unwrap();
    let inside = review.directory.join("inside-editor");
    fs::write(&inside, b"#!/bin/sh\nexit 83\n").unwrap();
    fs::set_permissions(&inside, fs::Permissions::from_mode(0o755)).unwrap();
    let alias = review.fixture.runtime.path().join("outside-alias");
    symlink(&inside, &alias).unwrap();

    for (selected, sentence) in [
        (
            review.fixture.runtime.path().join("missing-editor"),
            "The editor is not an executable file.",
        ),
        (non_executable, "The editor is not an executable file."),
        (
            review.fixture.runtime.path().to_path_buf(),
            "The editor is not an executable file.",
        ),
        (
            PathBuf::from("./relative-editor"),
            "The editor must name an absolute program.",
        ),
        (alias, "The editor must be outside the recorded checkout."),
    ] {
        let opened = review
            .cli
            .clone()
            .env("VISUAL", selected)
            .env("EDITOR", &fallback)
            .args(["ticket", "open", &review.card])
            .run();
        assert_eq!(opened.code(), 4, "{}", opened.output());
        assert_eq!(opened.stdout, "");
        assert_eq!(opened.stderr, format!("dudes: {sentence}\n"));
    }

    let valid = review
        .cli
        .env("VISUAL", fallback)
        .args(["ticket", "open", &review.card])
        .run();
    assert_eq!(valid.code(), 37, "{}", valid.output());
    assert_eq!(valid.stdout, "fallback ran\n");
    assert_eq!(valid.stderr, "");
}

#[test]
fn an_unsupported_visual_encoding_refuses_without_editor_fallback() {
    let review = review();
    let fallback = recorder(
        &review,
        "encoding-fallback",
        b"#!/bin/sh\nprintf 'fallback ran\\n'\nexit 37\n",
    );
    let opened = review
        .cli
        .clone()
        .env("VISUAL", OsString::from_vec(vec![0xff]))
        .env("EDITOR", &fallback)
        .args(["ticket", "open", &review.card])
        .run();

    assert_eq!(opened.code(), 4, "{}", opened.output());
    assert_eq!(opened.stdout, "");
    assert_eq!(
        opened.stderr,
        "dudes: The selected editor configuration is not valid UTF-8.\n"
    );

    let valid = review
        .cli
        .env("VISUAL", fallback)
        .args(["ticket", "open", &review.card])
        .run();
    assert_eq!(valid.code(), 37, "{}", valid.output());
    assert_eq!(valid.stdout, "fallback ran\n");
    assert_eq!(valid.stderr, "");
}

#[test]
fn an_explicit_editor_ignores_malformed_editor_configuration() {
    let review = review();
    let editor = recorder(
        &review,
        "explicit-editor",
        b"#!/bin/sh\nprintf 'argc=%s\\n' \"$#\"\nprintf '%s\\n' \"$@\"\nexit 37\n",
    );
    let opened = review
        .cli
        .env("VISUAL", OsString::from_vec(vec![0xff]))
        .env("EDITOR", "./invalid-selection --extra")
        .args(["ticket", "open", &review.card, "--editor"])
        .arg(editor)
        .run();

    assert_eq!(opened.code(), 37, "{}", opened.output());
    assert_eq!(
        opened.stdout,
        format!("argc=1\n{}\n", review.directory.display())
    );
    assert_eq!(opened.stderr, "");
}

#[test]
fn configured_shell_characters_remain_literal_argument_content() {
    let review = review();
    let editor = recorder(
        &review,
        "literal-editor",
        b"#!/bin/sh\nprintf 'argc=%s\\n' \"$#\"\nprintf '%s\\n' \"$@\"\nexit 37\n",
    );
    let opened = review
        .cli
        .env(
            "VISUAL",
            format!(
                r#"{} 'two words' "$NAME" $(printf ignored) *.rs \path"#,
                editor.display()
            ),
        )
        .args(["ticket", "open", &review.card])
        .run();

    assert_eq!(opened.code(), 37, "{}", opened.output());
    assert_eq!(
        opened.stdout,
        format!(
            "argc=8\n'two\nwords'\n\"$NAME\"\n$(printf\nignored)\n*.rs\n\\path\n{}\n",
            review.directory.display()
        )
    );
    assert_eq!(opened.stderr, "");
}

#[test]
fn print_and_json_bypass_malformed_editor_configuration() {
    let review = review();
    let cli = review
        .cli
        .env("VISUAL", OsString::from_vec(vec![0xff]))
        .env("EDITOR", "./invalid-selection --extra");
    let printed = cli
        .clone()
        .args(["ticket", "open", &review.card, "--print"])
        .run();
    assert_eq!(printed.code(), 0, "{}", printed.output());
    assert_eq!(printed.stdout, format!("{}\n", review.directory.display()));
    assert_eq!(printed.stderr, "");

    let machine = cli.args(["--json", "ticket", "open", &review.card]).run();
    assert_eq!(machine.code(), 0, "{}", machine.output());
    let checkout = machine.json::<TicketCheckoutResult>();
    assert_eq!(checkout.directory, review.directory);
    assert_eq!(checkout.worktree, None);
    assert_eq!(machine.stderr, "");
}

#[test]
fn ssh_sessions_print_for_both_configured_and_explicit_editor_selections() {
    let review = review();
    let editor = recorder(
        &review,
        "ssh-editor",
        b"#!/bin/sh\nprintf 'editor launched\\n'\nexit 37\n",
    );
    for marker in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"] {
        for explicit in [false, true] {
            let mut cli = review
                .cli
                .clone()
                .env(marker, "present")
                .env("VISUAL", OsString::from_vec(vec![0xff]))
                .env("EDITOR", "./invalid-selection")
                .args(["ticket", "open", &review.card]);
            if explicit {
                cli = cli.args(["--editor"]).arg(&editor);
            }
            let opened = cli.run();
            assert_eq!(opened.code(), 0, "{}", opened.output());
            assert_eq!(opened.stdout, format!("{}\n", review.directory.display()));
            assert_eq!(opened.stderr, "");
        }
    }
}

#[test]
fn an_editor_receives_no_minted_agent_token_or_control_socket() {
    let review = review();
    let shown = review
        .cli
        .clone()
        .args(["--json", "ticket", "show", &review.card])
        .run();
    assert_eq!(shown.code(), 0, "{}", shown.output());
    let owner = shown
        .json::<TicketReadResult>()
        .card
        .claimed_by
        .expect("the fixture ticket has an assigned agent");
    let agent = owner.to_string();
    let resumed = review.cli.clone().args(["resume", &agent]).run();
    assert_eq!(resumed.code(), 0, "{}", resumed.output());
    let token_hash =
        review
            .fixture
            .await_record("the owner's minted token", |record| match record {
                Record::HookTokenMinted { agent, token_hash } if *agent == owner => {
                    Some(token_hash.clone())
                }
                _ => None,
            });
    let token = review.running.tokens.token_of(&token_hash);
    review
        .fixture
        .await_record("the owner's fake turn spawn", |record| match record {
            Record::TurnSpawned { agent, .. } if *agent == owner => Some(()),
            _ => None,
        });
    let before = review.fixture.records();
    let editor = recorder(
        &review,
        "credential-check-editor",
        b"#!/bin/sh\ntoken_present=0\nsocket_present=0\nif [ \"${DUDES_TOKEN+x}\" = x ]; then token_present=1; fi\nif [ \"${DUDES_SOCK+x}\" = x ]; then socket_present=1; fi\nprintf 'token_present=%s\\nsocket_present=%s\\nargc=%s\\noperand=%s\\n' \"$token_present\" \"$socket_present\" \"$#\" \"$1\"\nexit 37\n",
    );
    let opened = review
        .cli
        .env("DUDES_TOKEN", token.expose())
        .args(["ticket", "open", &review.card, "--editor"])
        .arg(editor)
        .run();

    assert_eq!(opened.code, Some(37), "the editor owns the exit status");
    assert!(
        opened.stdout
            == format!(
                "token_present=0\nsocket_present=0\nargc=1\noperand={}\n",
                review.directory.display()
            ),
        "the editor receives one recorded operand and neither control variable"
    );
    assert!(opened.stderr.is_empty(), "the editor emits no error text");
    assert!(
        before == review.fixture.records(),
        "the editor handoff leaves the daemon's journal unchanged"
    );
}
