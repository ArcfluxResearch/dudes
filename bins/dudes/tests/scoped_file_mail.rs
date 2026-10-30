//! Scoped command transport checks against the shipped CLI.
#![cfg(any(target_os = "macos", target_os = "linux"))]

mod common;

use std::fs;
use std::io;
use std::os::unix::net::UnixListener;
use std::time::Duration;

use common::Cli;
use dudes::exit;
use dudes_types::{AgentId, MailId};

#[test]
fn scoped_commands_refuse_before_opening_the_mailbox() {
    let root = tempfile::tempdir().unwrap();
    let mailbox = root.path().join(AgentId::from_parts(1, 1).to_string());
    let run = Cli::bare()
        .env("HOME", root.path())
        .env("DUDES_MAILBOX", &mailbox)
        .env("DUDES_COMMAND_TRANSPORT", "file-mail")
        .args(["mail", "done", &MailId::from_parts(1, 1).to_string()])
        .run();
    assert_eq!(run.code(), exit::REFUSED, "{}", run.output());
    assert!(
        run.stderr
            .contains("dudes mail done is not yet available in a multi-repo ticket")
    );
    assert!(!mailbox.exists());
}

#[test]
fn scoped_socket_and_local_commands_refuse_without_connecting_or_writing() {
    let root = tempfile::Builder::new()
        .prefix("dudes-mail-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = root.path().join("door.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = root.path().join("operator.toml");
    fs::write(&config, "# sentinel operator file\n").unwrap();
    for (args, label) in [
        (vec!["ping"], "dudes ping"),
        (vec!["quickstart"], "dudes quickstart"),
        (vec!["start"], "dudes start"),
        (vec!["open"], "dudes open"),
        (vec!["ask", "--text", "question"], "dudes ask"),
        (vec!["memory", "read"], "dudes memory read"),
        (vec!["team"], "dudes team"),
        (vec!["whoami"], "dudes whoami"),
        (
            vec!["closing", "ack", "--announcement-lsn", "7"],
            "dudes closing ack",
        ),
        (
            vec!["project", "remove", "sentinel"],
            "dudes project remove",
        ),
        (vec!["config"], "dudes config"),
        (vec!["uninstall", "--yes", "--dry-run"], "dudes uninstall"),
        (vec!["upgrade", "--check"], "dudes upgrade"),
    ] {
        let run = Cli::new(&socket, root.path())
            .env("HOME", root.path())
            .env("DUDES_STATE_DIR", root.path().join("state"))
            .env("DUDES_CONFIG", &config)
            .env("DUDES_DAEMON", root.path().join("absent-daemon"))
            .env("DUDES_COMMAND_TRANSPORT", "file-mail")
            .args(args)
            .run();
        assert_eq!(run.code(), exit::REFUSED, "{label}: {}", run.output());
        assert!(
            run.stderr.contains(&format!(
                "{label} is not yet available in a multi-repo ticket"
            )),
            "{}",
            run.output()
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            "# sentinel operator file\n"
        );
        assert!(!root.path().join("state").exists());
        assert!(!root.path().join("operator.cred").exists());
    }
}

#[test]
fn scoped_parse_help_version_and_refusal_rendering_keep_their_exit_codes() {
    let cli = || Cli::bare().env("DUDES_COMMAND_TRANSPORT", "file-mail");
    for args in [["--help"], ["--version"]] {
        assert_eq!(cli().args(args).run().code(), exit::SUCCESS);
    }
    for args in [vec!["ask", "--help"], vec!["unknown"], vec!["mail", "done"]] {
        let run = cli().args(args).run();
        assert_eq!(run.code(), exit::USAGE, "{}", run.output());
        assert!(!run.stderr.contains("multi-repo ticket"));
    }
    let run = cli().args(["--json", "team"]).run();
    assert_eq!(run.code(), exit::REFUSED, "{}", run.output());
    let error: serde_json::Value = serde_json::from_str(&run.stderr).unwrap();
    assert_eq!(error["code"], -32004);
    assert_eq!(
        error["message"],
        "dudes team is not yet available in a multi-repo ticket"
    );
    assert_eq!(Cli::bare().args(["team"]).run().code(), exit::OTHER);
}

#[test]
fn scoped_ticket_notes_refuse_while_standard_input_is_still_open() {
    for args in [
        vec!["ticket", "add", "--title", "Title", "--note-file", "-"],
        vec![
            "ticket",
            "edit",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--note-file",
            "-",
        ],
    ] {
        let run = finish_with_open_input(
            Cli::bare()
                .env("DUDES_COMMAND_TRANSPORT", "file-mail")
                .args(args),
        );
        assert_eq!(run.code(), exit::REFUSED, "{}", run.output());
        assert!(
            run.stderr
                .contains("not yet available in a multi-repo ticket")
        );
        assert!(!run.stderr.contains("Reading the note"));
    }
}

#[test]
fn scoped_ticket_notes_refuse_missing_files_after_validating_syntax() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("absent-note.md");
    let path = missing.to_str().unwrap();
    for (args, label) in [
        (
            vec!["ticket", "add", "--title", "Title", "--note-file", path],
            "dudes ticket add",
        ),
        (
            vec![
                "ticket",
                "edit",
                "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "--note-file",
                path,
            ],
            "dudes ticket edit",
        ),
    ] {
        let run = Cli::bare()
            .env("DUDES_COMMAND_TRANSPORT", "file-mail")
            .args(&args)
            .run();
        assert_eq!(run.code(), exit::REFUSED, "{}", run.output());
        assert!(run.stderr.contains(&format!(
            "{label} is not yet available in a multi-repo ticket"
        )));
        let legacy = Cli::bare().args(args).run();
        assert_eq!(legacy.code(), exit::USAGE, "{}", legacy.output());
        assert!(legacy.stderr.contains("--note-file"));
    }
    for args in [
        vec!["ticket", "add", "--note-file", "-"],
        vec!["ticket", "edit", "invalid-card", "--note-file", "-"],
        vec![
            "ticket",
            "add",
            "--title",
            "Title",
            "--note",
            "inline",
            "--note-file",
            "-",
        ],
        vec![
            "ticket",
            "edit",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--note-file",
            "-",
            "--help",
        ],
    ] {
        let run = finish_with_open_input(
            Cli::bare()
                .env("DUDES_COMMAND_TRANSPORT", "file-mail")
                .args(args),
        );
        assert_eq!(run.code(), exit::USAGE, "{}", run.output());
        assert!(!run.stderr.contains("multi-repo ticket"));
    }
}

fn finish_with_open_input(cli: Cli) -> common::Run {
    let piped = cli.spawn_with_piped_stdin();
    let finished = piped.result.recv_timeout(Duration::from_secs(2));
    drop(piped.stdin);
    if finished.is_err() {
        let _ = piped.result.recv_timeout(Duration::from_secs(1));
    }
    piped.worker.join().unwrap();
    finished.expect("CLI must finish while note input is still open")
}
