//! Mailbox CLI integration checks.
#![cfg(any(target_os = "macos", target_os = "linux"))]

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Cli, Fixture, OpenRunner, SharedHost};
use dudes::args::Command;
use dudes::exit;
use dudes_proto::{MailFrom, MailTo, Record};
use dudes_types::{AgentId, MailId, MailState, ThreadId};
use serde_json::{Value, json};

struct Mailbox {
    root: tempfile::TempDir,
    path: PathBuf,
    agent: AgentId,
}

impl Mailbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let agent = AgentId::from_parts(1, 1);
        let path = root.path().join(agent.to_string());
        fs::create_dir_all(path.join("outbox")).unwrap();
        fs::create_dir_all(path.join("inbox/.done")).unwrap();
        Self { root, path, agent }
    }

    fn cli(&self) -> Cli {
        offline(&self.path)
    }

    fn marker(&self, id: MailId, body: &str) -> Value {
        json!({ "id":id, "from":{"kind":"agent","agent":AgentId::from_parts(1, 2)},
            "to":{"kind":"agent","agent":self.agent}, "act":"message", "subject":"subject",
            "body":body, "body_len":body.len() })
    }

    fn write(&self, id: MailId, marker: &Value) -> PathBuf {
        let path = self.path.join("inbox").join(format!("{id}.json"));
        fs::write(&path, serde_json::to_vec(marker).unwrap()).unwrap();
        path
    }
}

fn offline(path: &Path) -> Cli {
    Cli::bare()
        .env("DUDES_MAILBOX", path)
        .env("DUDES_SOCK", path.join("absent.sock"))
}

fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "owned mailbox observation did not arrive"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn hire(running: &common::Running, cwd: &Path) -> AgentId {
    let Command::Hire(params) = dudes::args::parse(&common::hire_args(cwd)).unwrap().command else {
        panic!("hire params")
    };
    match running.handle.ask(duded::ports::Ask::Hire(params)).unwrap() {
        duded::ports::Answered::Hired(result) => result.agent,
        other => panic!("hire returned {other:?}"),
    }
}

#[test]
fn send_is_offline_exact_synced_private_and_has_no_sender_metadata() {
    let mailbox = Mailbox::new();
    let target = AgentId::from_parts(1, 2).to_string();
    let run = mailbox
        .cli()
        .args([
            "mail",
            "send",
            "--to",
            &target,
            "--act",
            "request",
            "--subject",
            "--json",
            "--body",
            "exact $body\n🦀",
        ])
        .run();
    assert_eq!(run.code(), exit::SUCCESS, "{}", run.output());
    let files: Vec<_> = fs::read_dir(mailbox.path.join("outbox"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1);
    let id: MailId = files[0]
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        files[0].file_name().unwrap().to_str().unwrap(),
        format!("{id}.json")
    );
    assert_eq!(fs::metadata(&files[0]).unwrap().mode() & 0o777, 0o600);
    let request: Value = serde_json::from_slice(&fs::read(&files[0]).unwrap()).unwrap();
    assert_eq!(
        request,
        json!({"to":{"agent":target},"act":"request","subject":"--json","body":"exact $body\n🦀"})
    );
    assert_eq!(run.stdout, format!("mail queued {id}\n"));
    assert!(run.stderr.is_empty());
}

#[test]
fn a_request_offers_a_ready_reply_but_a_message_does_not() {
    let mailbox = Mailbox::new();
    let id = MailId::from_parts(1, 18);
    let mut marker = mailbox.marker(id, "Which branch should I use?");
    marker["act"] = json!("request");
    marker["thread"] = json!(ThreadId::from_parts(1, 3));
    marker["thread_seq"] = json!(1);
    marker["members"] = json!([mailbox.agent, AgentId::from_parts(1, 2)]);
    mailbox.write(id, &marker);
    let request = mailbox.cli().args(["mail", "read"]).run();
    assert_eq!(request.code(), 0, "{}", request.output());
    assert!(request.stdout.contains("act request\n"));
    assert!(request.stdout.contains(&format!(
        "dudes mail send --reply-to {id} --act message --body 'Your reply'"
    )));
    for field in ["thread", "thread_seq", "members"] {
        marker.as_object_mut().unwrap().remove(field);
    }
    mailbox.write(id, &marker);
    let legacy = mailbox.cli().args(["mail", "read"]).run();
    assert_eq!(legacy.code(), 0, "{}", legacy.output());
    assert!(
        legacy
            .stdout
            .contains("reply unavailable: this mail has no thread\n")
    );
    assert!(!legacy.stdout.contains("dudes mail send --reply-to"));
    marker["act"] = json!("message");
    mailbox.write(id, &marker);
    let message = mailbox.cli().args(["mail", "read"]).run();
    assert_eq!(message.code(), 0, "{}", message.output());
    assert!(message.stdout.contains("act message\n"));
    assert!(!message.stdout.contains("dudes mail send --reply-to"));
}

#[test]
fn list_shows_current_inbox_metadata_without_bodies() {
    let mailbox = Mailbox::new();
    let id = MailId::from_parts(1, 19);
    let marker = mailbox.marker(id, "body stays out of the list");
    mailbox.write(id, &marker);
    let listed = mailbox.cli().args(["mail", "list"]).run();
    assert_eq!(listed.code(), 0, "{}", listed.output());
    assert!(listed.stdout.contains(&format!("mail {id}\n")));
    assert!(listed.stdout.contains("act message\n"));
    assert!(listed.stdout.contains("subject subject\n"));
    assert!(!listed.stdout.contains("body stays out of the list"));
    let read = mailbox.cli().args(["mail", "read"]).run();
    assert!(read.stdout.contains("body stays out of the list"));
}

#[test]
fn a_reply_can_derive_its_subject_from_a_pending_or_acknowledged_parent() {
    for acknowledged in [false, true] {
        let mailbox = Mailbox::new();
        let id = MailId::from_parts(1, 20);
        let mut marker = mailbox.marker(id, "");
        marker["subject"] = json!("Check 🦀 input");
        mailbox.write(id, &marker);
        if acknowledged {
            let done = mailbox.cli().args(["mail", "done", &id.to_string()]).run();
            assert_eq!(done.code(), 0, "{}", done.output());
        }
        let sent = mailbox
            .cli()
            .args([
                "mail",
                "send",
                "--reply-to",
                &id.to_string(),
                "--act",
                "message",
                "--body",
                "Use main.",
            ])
            .run();
        assert_eq!(sent.code(), 0, "{}", sent.output());
        let path = fs::read_dir(mailbox.path.join("outbox"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let request: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(
            request,
            json!({"in_reply_to":id,"act":"message", "subject":"Re: Check 🦀 input", "body":"Use main."})
        );
    }
    let mailbox = Mailbox::new();
    let missing = mailbox
        .cli()
        .args([
            "mail",
            "send",
            "--reply-to",
            &MailId::from_parts(1, 21).to_string(),
            "--act",
            "message",
            "--body",
            "No parent.",
        ])
        .run();
    assert_eq!(missing.code(), 4, "{}", missing.output());
    assert!(missing.stderr.contains("provide --subject explicitly"));
    assert_eq!(
        fs::read_dir(mailbox.path.join("outbox")).unwrap().count(),
        0
    );
    let new_thread = mailbox
        .cli()
        .args([
            "mail",
            "send",
            "--to",
            &AgentId::from_parts(1, 2).to_string(),
            "--act",
            "message",
        ])
        .run();
    assert_eq!(new_thread.code(), 1);
    assert!(new_thread.stderr.contains("--subject"));
    let explicit = mailbox
        .cli()
        .args([
            "mail",
            "send",
            "--reply-to",
            &MailId::from_parts(1, 21).to_string(),
            "--act",
            "message",
            "--subject",
            "Chosen subject",
        ])
        .run();
    assert_eq!(explicit.code(), 0, "{}", explicit.output());
}

fn start_mail_pair(
    fixture: &Fixture,
    running: &common::Running,
    settings: &duded::settings::Settings,
) -> [AgentId; 2] {
    let sender = hire(running, fixture.state.path());
    let recipient = hire(running, fixture.state.path());
    for agent in [sender, recipient] {
        let goal = Cli::new(&settings.socket(), &settings.runtime_dir)
            .args(common::goal_args(
                &agent.to_string(),
                "exchange the fixture mail",
            ))
            .run();
        assert_eq!(goal.code(), exit::SUCCESS, "{}", goal.output());
        fixture.await_record("owned mail sender context", |record| {
            matches!(record, Record::TurnSpawned { agent: owner, .. } if *owner == agent)
                .then_some(())
        });
    }
    [sender, recipient]
}

fn round_trip(body: &str) {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.journal.checkpoint_every = 0;
    let mut running = common::start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let [sender, recipient] = start_mail_pair(&fixture, &running, &settings);
    let source = fixture.state.path().join("agents").join(sender.to_string());
    let destination = fixture
        .state
        .path()
        .join("agents")
        .join(recipient.to_string());
    wait(|| source.join("outbox").is_dir() && destination.join("inbox/.done").is_dir());
    let sent = offline(&source)
        .args([
            "mail",
            "send",
            "--to",
            &recipient.to_string(),
            "--act",
            "request",
            "--subject",
            "original",
            "--body",
            body,
        ])
        .run();
    assert_eq!(sent.code(), 0, "{}", sent.output());
    let (id, thread, members) =
        fixture.await_record("structural sender mail", |record| match record {
            Record::Mail {
                id,
                from: MailFrom::Agent { agent: from },
                to: MailTo::Agent { agent: to },
                thread,
                members,
                body_len,
                body: inline,
                body_blake3,
                ..
            } if *from == sender && *to == recipient => {
                assert_eq!(*body_len, body.len() as u64);
                if body.len() <= 4_096 {
                    assert_eq!(inline.as_deref(), Some(body));
                } else {
                    assert!(inline.is_none());
                    assert_eq!(
                        body_blake3.as_deref(),
                        Some(dudes_journal::mail_body_digest(body.as_bytes()).as_str())
                    );
                }
                Some((*id, thread.unwrap(), members.clone()))
            }
            _ => None,
        });
    read_delivered(&destination, id, body);
    let reply = offline(&destination)
        .args([
            "mail",
            "send",
            "--reply-to",
            &id.to_string(),
            "--act",
            "message",
            "--body",
            "answer",
        ])
        .run();
    assert_eq!(reply.code(), 0, "{}", reply.output());
    fixture.await_record("reply in original thread", |record| match record {
        Record::Mail {
            from: MailFrom::Agent { agent: from },
            to: MailTo::Agent { agent: to },
            thread: Some(actual),
            thread_seq,
            members: actual_members,
            body: Some(body),
            subject,
            ..
        } if *from == recipient && *to == sender => {
            assert_eq!(*actual, thread);
            assert_eq!(*thread_seq, 2);
            assert_eq!(actual_members, &members);
            assert_eq!(body, "answer");
            assert_eq!(subject, "Re: original");
            Some(())
        }
        _ => None,
    });
    acknowledge_once(&fixture, &destination, id);
    assert!(
        !fixture.records().iter().any(|record| {
            matches!(record, Record::TurnEnded { agent, .. } if [sender, recipient].contains(agent))
        }),
        "both owned sender contexts remain held through the round trip"
    );
    running.stop();
}

fn read_delivered(destination: &Path, id: MailId, body: &str) {
    let marker = destination.join("inbox").join(format!("{id}.json"));
    wait(|| marker.exists());
    let read = offline(destination).args(["mail", "read"]).run();
    assert_eq!(read.code(), 0, "{}", read.output());
    assert!(read.stdout.contains(&format!("mail {id}")));
    assert!(read.stdout.contains("subject original"));
    assert!(read.stdout.contains("act request\n"));
    assert!(read.stdout.contains(&format!(
        "dudes mail send --reply-to {id} --act message --body 'Your reply'"
    )));
    assert!(read.stdout.contains(body));
    let unread = offline(destination)
        .args(["mail", "read", "--unread"])
        .run();
    assert_eq!(unread.code(), 0);
    assert_eq!(unread.stdout, read.stdout);
}

fn acknowledge_once(fixture: &Fixture, destination: &Path, id: MailId) {
    let marker = destination.join("inbox").join(format!("{id}.json"));
    let done = offline(destination)
        .args(["mail", "done", &id.to_string()])
        .run();
    assert_eq!(done.code(), 0, "{}", done.output());
    assert_eq!(done.stdout, format!("mail done {id}\n"));
    fixture.await_record("existing mailbox acknowledgement", |record| matches!(record, Record::MailState { id: actual, state: MailState::Acked } if *actual == id).then_some(()));
    wait(|| {
        !marker.exists()
            && !destination
                .join("inbox/.done")
                .join(format!("{id}.json"))
                .exists()
    });
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fixture.records().iter().filter(|record| matches!(record, Record::MailState { id: actual, state: MailState::Acked } if *actual == id)).count(), 1);
}

#[test]
fn inline_boundary_cli_round_trip_replies_and_acks_once() {
    round_trip(&"x".repeat(4_096));
}

#[test]
fn referenced_boundary_cli_round_trip_replies_and_acks_once() {
    round_trip(&"x".repeat(4_097));
}

#[test]
fn legacy_read_is_terminal_safe_and_done_moves_the_original_inode_without_clobber() {
    let mailbox = Mailbox::new();
    let id = MailId::from_parts(1, 9);
    let mut marker = mailbox.marker(id, "safe\u{1b}[2J\nbody");
    marker["subject"] = json!("subject\u{202e}");
    let source = mailbox.write(id, &marker);
    let inode = fs::metadata(&source).unwrap().ino();
    let read = mailbox.cli().args(["mail", "read"]).run();
    assert_eq!(read.code(), 0, "{}", read.output());
    assert!(read.stdout.contains("safe\\u{1b}[2J\\u{a}body"));
    assert!(read.stdout.contains("subject\\u{202e}"));
    assert!(!read.stdout.contains('\u{1b}'));
    let target = mailbox.path.join("inbox/.done").join(format!("{id}.json"));
    fs::write(&target, "planted destination").unwrap();
    let refused = mailbox.cli().args(["mail", "done", &id.to_string()]).run();
    assert_eq!(refused.code(), 4);
    assert_eq!(fs::read_to_string(&target).unwrap(), "planted destination");
    assert_eq!(fs::metadata(&source).unwrap().ino(), inode);
    fs::remove_file(&target).unwrap();
    let done = mailbox.cli().args(["mail", "done", &id.to_string()]).run();
    assert_eq!(done.code(), 0, "{}", done.output());
    assert!(!source.exists());
    assert_eq!(fs::metadata(&target).unwrap().ino(), inode);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&target).unwrap()).unwrap(),
        marker
    );
    assert_eq!(
        mailbox.cli().args(["mail", "read"]).run().stdout,
        "no unread mail\n"
    );
}

#[test]
fn corrupt_marker_body_and_recipient_are_refused_without_body_output() {
    let mailbox = Mailbox::new();
    let id = MailId::from_parts(1, 10);
    let base = mailbox.marker(id, "suspect body");
    let mut cases = Vec::new();
    let mut wrong_id = base.clone();
    wrong_id["id"] = json!(MailId::from_parts(1, 11));
    cases.push(wrong_id);
    let mut wrong_recipient = base.clone();
    wrong_recipient["to"]["agent"] = json!(AgentId::from_parts(1, 2));
    cases.push(wrong_recipient);
    let mut wrong_len = base.clone();
    wrong_len["body_len"] = json!(1);
    cases.push(wrong_len);
    let mut split = base.clone();
    split["body_path"] = json!("escape");
    cases.push(split);
    for marker in cases {
        mailbox.write(id, &marker);
        let read = mailbox.cli().args(["mail", "read"]).run();
        assert_eq!(read.code(), 4, "{}", read.output());
        assert!(read.stdout.is_empty());
        assert!(!read.output().contains("suspect body"));
    }
    fs::write(
        mailbox.path.join("inbox").join(format!("{id}.json")),
        "not json",
    )
    .unwrap();
    assert_eq!(mailbox.cli().args(["mail", "read"]).run().code(), 4);
}

#[test]
fn referenced_read_requires_own_canonical_path_length_digest_and_utf8() {
    let mailbox = Mailbox::new();
    let id = MailId::from_parts(1, 12);
    let body = "verified body ".repeat(400);
    let body_file = mailbox.path.join("inbox").join(format!("{id}.body"));
    fs::write(&body_file, body.as_bytes()).unwrap();
    let canonical = format!("agents/{}/inbox/{id}.body", mailbox.agent);
    let mut marker = mailbox.marker(id, "");
    marker["body"] = Value::Null;
    marker["body_len"] = json!(body.len());
    marker["body_path"] = json!(canonical);
    marker["body_blake3"] = json!(dudes_journal::mail_body_digest(body.as_bytes()));
    mailbox.write(id, &marker);
    let good = mailbox.cli().args(["mail", "read"]).run();
    assert_eq!(good.code(), 0, "{}", good.output());
    assert!(good.stdout.contains(&body));
    for path in [
        "../escape".to_owned(),
        body_file.to_string_lossy().into_owned(),
        format!("agents/{}/inbox/{id}.body", AgentId::from_parts(1, 2)),
        format!(
            "agents/{}/inbox/{}.body",
            mailbox.agent,
            MailId::from_parts(1, 13)
        ),
    ] {
        marker["body_path"] = json!(path);
        mailbox.write(id, &marker);
        let read = mailbox.cli().args(["mail", "read"]).run();
        assert_eq!(read.code(), 4);
        assert!(read.stdout.is_empty());
    }
    marker["body_path"] = json!(canonical);
    marker["body_len"] = json!(body.len() + 1);
    mailbox.write(id, &marker);
    let bad_len = mailbox.cli().args(["mail", "read"]).run();
    assert_eq!(bad_len.code(), 4);
    assert!(bad_len.stdout.is_empty());
    marker["body_len"] = json!(body.len());
    marker["body_blake3"] = json!("0".repeat(64));
    mailbox.write(id, &marker);
    let bad_hash = mailbox.cli().args(["mail", "read"]).run();
    assert_eq!(bad_hash.code(), 4);
    assert!(bad_hash.stdout.is_empty());
    let bytes = vec![0xff; 4_097];
    fs::write(&body_file, &bytes).unwrap();
    marker["body_len"] = json!(bytes.len());
    marker["body_blake3"] = json!(dudes_journal::mail_body_digest(&bytes));
    mailbox.write(id, &marker);
    let bad_utf8 = mailbox.cli().args(["mail", "read"]).run();
    assert_eq!(bad_utf8.code(), 4);
    assert!(bad_utf8.stdout.is_empty());
}

#[test]
fn referenced_body_links_and_byte_caps_refuse_before_output() {
    let mailbox = Mailbox::new();
    let id = MailId::from_parts(1, 16);
    let bytes = vec![b'x'; 4_097];
    let outside = mailbox.root.path().join("body");
    fs::write(&outside, &bytes).unwrap();
    let body = mailbox.path.join("inbox").join(format!("{id}.body"));
    let mut marker = mailbox.marker(id, "");
    marker["body"] = Value::Null;
    marker["body_len"] = json!(bytes.len());
    marker["body_path"] = json!(format!("agents/{}/inbox/{id}.body", mailbox.agent));
    marker["body_blake3"] = json!(dudes_journal::mail_body_digest(&bytes));
    mailbox.write(id, &marker);
    symlink(&outside, &body).unwrap();
    let linked = mailbox.cli().args(["mail", "read"]).run();
    assert_eq!(linked.code(), 4);
    assert!(linked.stdout.is_empty());
    fs::remove_file(&body).unwrap();
    fs::hard_link(&outside, &body).unwrap();
    assert_eq!(mailbox.cli().args(["mail", "read"]).run().code(), 4);
    fs::remove_file(&body).unwrap();
    fs::write(&body, vec![b'x'; 1_048_577]).unwrap();
    assert_eq!(mailbox.cli().args(["mail", "read"]).run().code(), 4);
    marker["subject"] = json!("x".repeat(65_536));
    mailbox.write(id, &marker);
    let oversized = mailbox.cli().args(["mail", "read"]).run();
    assert_eq!(oversized.code(), 4);
    assert!(oversized.stdout.is_empty());
}

#[test]
fn symlinked_outbox_done_and_agent_directory_refuse_without_writing() {
    let mailbox = Mailbox::new();
    let id = MailId::from_parts(1, 17);
    let marker = mailbox.marker(id, "unchanged");
    let source = mailbox.write(id, &marker);
    let done = mailbox.path.join("inbox/.done");
    let moved = mailbox.path.join("saved-done");
    fs::rename(&done, &moved).unwrap();
    symlink(&moved, &done).unwrap();
    assert_eq!(
        mailbox
            .cli()
            .args(["mail", "done", &id.to_string()])
            .run()
            .code(),
        4
    );
    assert!(source.exists());
    assert_eq!(fs::read_dir(&moved).unwrap().count(), 0);
    let outbox = mailbox.path.join("outbox");
    let moved = mailbox.path.join("saved-outbox");
    fs::rename(&outbox, &moved).unwrap();
    symlink(&moved, &outbox).unwrap();
    let target = AgentId::from_parts(1, 2).to_string();
    assert_eq!(
        mailbox
            .cli()
            .args([
                "mail",
                "send",
                "--to",
                &target,
                "--act",
                "message",
                "--subject",
                "no write"
            ])
            .run()
            .code(),
        4
    );
    assert_eq!(fs::read_dir(&moved).unwrap().count(), 0);
    let moved = mailbox.root.path().join("saved-agent");
    fs::rename(&mailbox.path, &moved).unwrap();
    symlink(&moved, &mailbox.path).unwrap();
    assert_eq!(mailbox.cli().args(["mail", "read"]).run().code(), 4);
}

#[test]
fn links_and_symlinked_mailbox_components_are_never_followed() {
    let mailbox = Mailbox::new();
    let id = MailId::from_parts(1, 14);
    let marker = mailbox.marker(id, "outside secret");
    let source = mailbox.write(id, &marker);
    let outside = mailbox.root.path().join("outside.json");
    fs::rename(&source, &outside).unwrap();
    symlink(&outside, &source).unwrap();
    assert_eq!(mailbox.cli().args(["mail", "read"]).run().code(), 4);
    fs::remove_file(&source).unwrap();
    fs::hard_link(&outside, &source).unwrap();
    assert_eq!(mailbox.cli().args(["mail", "read"]).run().code(), 4);
    fs::remove_file(&source).unwrap();
    let inbox = mailbox.path.join("inbox");
    let moved = mailbox.path.join("saved-inbox");
    fs::rename(&inbox, &moved).unwrap();
    symlink(&moved, &inbox).unwrap();
    assert_eq!(mailbox.cli().args(["mail", "read"]).run().code(), 4);
    assert_eq!(
        fs::read_to_string(&outside).unwrap(),
        serde_json::to_string(&marker).unwrap()
    );
}

#[test]
fn read_distinguishes_raw_scan_budget_from_marker_slots() {
    let mailbox = Mailbox::new();
    for n in 0..100 {
        fs::write(mailbox.path.join("inbox").join(format!("junk-{n}")), "").unwrap();
    }
    let id = MailId::from_parts(1, 15);
    mailbox.write(id, &mailbox.marker(id, "bounded visible mail"));
    let read = mailbox.cli().args(["mail", "read"]).run();
    assert_eq!(read.code(), 0, "{}", read.output());
    assert!(read.stdout.contains("bounded visible mail"));
    for n in 100..1_025 {
        fs::write(mailbox.path.join("inbox").join(format!("junk-{n}")), "").unwrap();
    }
    let overflow = mailbox.cli().args(["mail", "read"]).run();
    assert_eq!(overflow.code(), 4);
    assert!(overflow.stdout.is_empty());
    let mailbox = Mailbox::new();
    for n in 1..=33 {
        let id = MailId::from_parts(1, n);
        mailbox.write(id, &mailbox.marker(id, "marker cap"));
    }
    assert_eq!(mailbox.cli().args(["mail", "read"]).run().code(), 4);
}

#[test]
fn missing_empty_mailbox_and_help_complete_without_socket_or_file_actions() {
    let missing = Cli::bare().args(["mail", "read"]).run();
    assert_eq!(missing.code(), 4);
    assert!(missing.stderr.contains("DUDES_MAILBOX is not set"));
    let empty = Cli::bare()
        .env("DUDES_MAILBOX", "")
        .args(["mail", "read"])
        .run();
    assert_eq!(empty.code(), 4);
    assert!(empty.stderr.contains("DUDES_MAILBOX is empty"));
    let mailbox = Mailbox::new();
    let help = mailbox.cli().args(["mail", "send", "--help"]).run();
    assert_eq!(help.code(), 1);
    assert!(help.stdout.contains("mail send"));
    assert!(help.stderr.is_empty());
    assert_eq!(
        fs::read_dir(mailbox.path.join("outbox")).unwrap().count(),
        0
    );
    let target = AgentId::from_parts(1, 2).to_string();
    let literal = mailbox
        .cli()
        .args([
            "mail",
            "send",
            "--to",
            &target,
            "--act",
            "message",
            "--subject",
            "--help",
            "--body",
            "--",
        ])
        .run();
    assert_eq!(literal.code(), 0, "{}", literal.output());
}

#[test]
fn public_rpc_digest_matches_the_existing_journal_body_digest() {
    for bytes in [
        b"".as_slice(),
        b"exact\x00bytes".as_slice(),
        "🦀".as_bytes(),
    ] {
        let digest = dudes_rpc::conditioned::payload_digest(bytes);
        assert_eq!(digest.decoded_len, bytes.len() as u64);
        assert_eq!(digest.blake3, dudes_journal::mail_body_digest(bytes));
    }
}
