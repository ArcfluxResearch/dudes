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

fn mailbox() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let mailbox = root.path().join(AgentId::from_parts(1, 1).to_string());
    fs::create_dir_all(mailbox.join("outbox")).unwrap();
    fs::create_dir_all(mailbox.join("inbox/.done")).unwrap();
    (root, mailbox)
}

fn requests(mailbox: &std::path::Path) -> Vec<(String, serde_json::Value)> {
    let mut found: Vec<_> = fs::read_dir(mailbox.join("outbox"))
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap(),
            )
        })
        .collect();
    found.sort_by(|left, right| left.0.cmp(&right.0));
    found
}

#[test]
fn scoped_acknowledgements_queue_requests_in_the_outbox_and_say_so() {
    let (root, mailbox) = mailbox();
    let marker = MailId::from_parts(1, 1);
    fs::write(mailbox.join("inbox").join(format!("{marker}.json")), "{}").unwrap();
    let cli = || {
        Cli::bare()
            .env("HOME", root.path())
            .env("DUDES_MAILBOX", &mailbox)
            .env("DUDES_COMMAND_TRANSPORT", "file-mail")
    };
    let done = cli().args(["mail", "done", &marker.to_string()]).run();
    assert_eq!(done.code(), exit::SUCCESS, "{}", done.output());
    assert!(done.stdout.contains("queued"), "{}", done.output());
    assert!(!done.stdout.contains("mail done 0"), "{}", done.output());
    let ack = cli()
        .args(["closing", "ack", "--announcement-lsn", "7"])
        .run();
    assert_eq!(ack.code(), exit::SUCCESS, "{}", ack.output());
    assert!(ack.stdout.contains("queued"), "{}", ack.output());
    assert!(!ack.stdout.contains("closing time acknowledged"));
    let queued = requests(&mailbox);
    assert_eq!(queued.len(), 2, "{queued:?}");
    assert!(
        queued
            .iter()
            .all(|(name, _)| name.ends_with(".control.json"))
    );
    let bodies: Vec<_> = queued.iter().map(|(_, body)| body.clone()).collect();
    assert!(bodies.contains(&serde_json::json!({"request":"mail_done","marker":marker})));
    assert!(bodies.contains(&serde_json::json!({"request":"closing_ack","announcement":7})));
    assert!(
        mailbox
            .join("inbox")
            .join(format!("{marker}.json"))
            .exists()
    );
    assert!(
        !mailbox
            .join("inbox/.done")
            .join(format!("{marker}.json"))
            .exists()
    );
}

#[test]
fn scoped_acknowledgement_requests_stop_when_the_outbox_is_full() {
    let (root, mailbox) = mailbox();
    for n in 0..64 {
        fs::write(mailbox.join("outbox").join(format!("filler-{n}")), "").unwrap();
    }
    let run = Cli::bare()
        .env("HOME", root.path())
        .env("DUDES_MAILBOX", &mailbox)
        .env("DUDES_COMMAND_TRANSPORT", "file-mail")
        .args(["closing", "ack", "--announcement-lsn", "7"])
        .run();
    assert_ne!(run.code(), exit::SUCCESS, "{}", run.output());
    assert!(run.stderr.contains("outbox is full"), "{}", run.output());
    assert_eq!(fs::read_dir(mailbox.join("outbox")).unwrap().count(), 64);
}

#[test]
fn legacy_mail_done_still_moves_the_marker_itself() {
    let (root, mailbox) = mailbox();
    let marker = MailId::from_parts(1, 2);
    let body = serde_json::json!({
        "id": marker,
        "from": {"kind": "human"},
        "to": {"kind": "agent", "agent": AgentId::from_parts(1, 1)},
        "act": "message",
        "subject": "subject",
        "body_len": 0
    });
    fs::write(
        mailbox.join("inbox").join(format!("{marker}.json")),
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    let run = Cli::bare()
        .env("HOME", root.path())
        .env("DUDES_MAILBOX", &mailbox)
        .args(["mail", "done", &marker.to_string()])
        .run();
    assert_eq!(run.code(), exit::SUCCESS, "{}", run.output());
    assert!(run.stdout.contains(&format!("mail done {marker}")));
    assert!(
        mailbox
            .join("inbox/.done")
            .join(format!("{marker}.json"))
            .exists()
    );
    assert!(requests(&mailbox).is_empty());
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
