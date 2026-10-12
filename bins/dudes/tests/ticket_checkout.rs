//! Recorded lease validation through the operator CLI and the daemon's production socket.
#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Cli, Fixture, OpenRunner, Running, SharedHost, start};
use dudes_proto::{Actor, AgentBudget, Entry, Predicate, Record, TicketCheck};
use dudes_types::{AgentId, CardId, GitCommit, Provider, Seq, Timestamp, WorktreeId};

fn git(repo: &Path, args: &[OsString]) -> String {
    let environment = BTreeMap::from([
        (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
        (
            OsString::from("GIT_CONFIG_GLOBAL"),
            OsString::from("/dev/null"),
        ),
    ]);
    let output = common::process(
        Path::new("/usr/bin/git").as_os_str(),
        args,
        Some(&environment),
    )
    .current_dir(repo)
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn strings(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

struct Lease {
    _running: Running,
    fixture: Fixture,
    cli: Cli,
    origin: PathBuf,
    directory: PathBuf,
    worktree: WorktreeId,
    card: CardId,
}

fn hired(agent: AgentId, directory: &Path, worktree: WorktreeId) -> Record {
    Record::AgentHired {
        id: agent,
        name: "Kim".into(),
        character: None,
        accent: None,
        role: "engineer".into(),
        department: "engineering".into(),
        provider: Provider::Claude,
        cwd: directory.to_path_buf(),
        workspace: None,
        budget: AgentBudget {
            usd_per_day: None,
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        trusted: false,
        worktree: Some(worktree),
        mcp_servers: Vec::new(),
        browser_onboarding: None,
    }
}

fn lease() -> Lease {
    let fixture = Fixture::new();
    let origin = fixture.state.path().join("origin");
    fs::create_dir(&origin).unwrap();
    let origin = origin.canonicalize().unwrap();
    git(&origin, &strings(&["init", "--quiet"]));
    git(
        &origin,
        &strings(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Initial",
        ]),
    );
    let base = GitCommit::parse(git(&origin, &strings(&["rev-parse", "HEAD"])).trim()).unwrap();
    let worktree = WorktreeId::from_bytes([0x11; 16]);
    let directory = origin.parent().unwrap().join(worktree.to_string());
    let mut args = strings(&["worktree", "add", "--quiet", "--detach"]);
    args.push(directory.clone().into_os_string());
    git(&origin, &args);
    let card = CardId::from_parts(4, 1);
    let agent = AgentId::from_parts(0, 1);
    let hired = hired(agent, &directory, worktree);
    let records = [
        Record::WorktreeLeased {
            worktree,
            origin: origin.clone(),
            path: directory.clone(),
            branch: format!("dudes/{}", worktree.short()),
            base,
            from: None,
        },
        hired,
        Record::TicketCreated {
            card,
            title: "Review the lease".into(),
            description: String::new(),
            department: "engineering".into(),
            project: "service".into(),
            done_when: TicketCheck {
                id: "default".into(),
                label: "Checks pass".into(),
                predicate: Predicate::Command {
                    argv: vec!["/usr/bin/true".into()],
                    timeout_s: 10,
                },
            },
            by: Actor::Peer { pid: 1, uid: None },
        },
        Record::TicketAssigned {
            card,
            agent,
            by: Actor::Peer { pid: 1, uid: None },
            message: None,
            expected_seq: Seq::ZERO,
        },
    ];
    let settings = fixture.settings();
    {
        let (mut writer, _) = dudes_journal::open(
            &settings.state_dir.join(duded::boot::JOURNAL_DIR),
            settings.journal,
        )
        .unwrap();
        for record in records {
            writer
                .append(&Entry {
                    at: Timestamp::from_unix_millis(1_000),
                    record,
                })
                .unwrap();
        }
    }
    let running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir);
    Lease {
        _running: running,
        fixture,
        cli,
        origin,
        directory,
        worktree,
        card,
    }
}

#[test]
fn the_recorded_lease_is_printed_without_appending_or_guessing_the_origin() {
    let lease = lease();
    let before = lease.fixture.records();
    let printed = lease
        .cli
        .clone()
        .args(["ticket", "open", &lease.card.to_string(), "--print"])
        .run();
    assert_eq!(printed.code(), 0, "{}", printed.output());
    assert_eq!(printed.stdout, format!("{}\n", lease.directory.display()));
    assert_eq!(printed.stderr, "");
    let machine = lease
        .cli
        .clone()
        .args(["--json", "ticket", "open", &lease.card.to_string()])
        .run();
    assert_eq!(machine.code(), 0, "{}", machine.output());
    assert_eq!(
        machine.json::<serde_json::Value>(),
        serde_json::json!({"directory": lease.directory, "worktree": {"worktree": lease.worktree, "origin": lease.origin}})
    );
    assert_eq!(lease.fixture.records(), before);
}

#[test]
fn a_gitfile_repointed_to_another_worktree_is_refused_before_printing() {
    let lease = lease();
    let other = lease.origin.parent().unwrap().join("another-tree");
    let mut args = strings(&["worktree", "add", "--quiet", "--detach"]);
    args.push(other.clone().into_os_string());
    git(&lease.origin, &args);
    fs::copy(other.join(".git"), lease.directory.join(".git")).unwrap();
    let before = lease.fixture.records();
    let printed = lease
        .cli
        .args(["ticket", "open", &lease.card.to_string(), "--print"])
        .run();
    assert_eq!(printed.code(), 4, "{}", printed.output());
    assert_eq!(printed.stdout, "");
    assert_eq!(
        printed.stderr,
        "dudes: The recorded checkout directory is no longer available.\n"
    );
    assert_eq!(lease.fixture.records(), before);
}

#[test]
fn a_commondir_repointed_away_from_the_recorded_origin_is_refused() {
    let lease = lease();
    let other = lease.fixture.runtime.path().join("other-origin");
    fs::create_dir(&other).unwrap();
    git(&other, &strings(&["init", "--quiet"]));
    let admin = lease
        .origin
        .join(".git/worktrees")
        .join(lease.worktree.to_string());
    fs::write(
        admin.join("commondir"),
        format!("{}\n", other.join(".git").display()),
    )
    .unwrap();
    let before = lease.fixture.records();
    let printed = lease
        .cli
        .args(["ticket", "open", &lease.card.to_string(), "--print"])
        .run();
    assert_eq!(printed.code(), 4, "{}", printed.output());
    assert_eq!(printed.stdout, "");
    assert_eq!(lease.fixture.records(), before);
}

#[test]
fn lease_files_and_administrative_directories_must_not_be_symlinks() {
    for component in ["gitfile", "commondir", "admin", "common", "tree"] {
        let lease = lease();
        let admin = lease
            .origin
            .join(".git/worktrees")
            .join(lease.worktree.to_string());
        let target = match component {
            "gitfile" => lease.directory.join(".git"),
            "commondir" => admin.join("commondir"),
            "admin" => admin,
            "common" => lease.origin.join(".git"),
            "tree" => lease.directory.clone(),
            _ => unreachable!(),
        };
        let relocated = lease.fixture.runtime.path().join("relocated");
        fs::rename(&target, &relocated).unwrap();
        std::os::unix::fs::symlink(&relocated, &target).unwrap();
        let before = lease.fixture.records();
        let printed = lease
            .cli
            .args(["ticket", "open", &lease.card.to_string(), "--print"])
            .run();
        assert_eq!(printed.code(), 4, "{component}: {}", printed.output());
        assert_eq!(printed.stdout, "", "{component}");
        assert_eq!(lease.fixture.records(), before);
    }
}

#[test]
fn malformed_or_oversized_lease_link_files_are_refused() {
    for (name, bytes) in [
        (".git", Vec::new()),
        (".git", b"gitdir: /one\n/another\n".to_vec()),
        (".git", b"gitdir: /one\0\n".to_vec()),
        ("commondir", vec![b'x'; 4097]),
    ] {
        let lease = lease();
        let path = if name == ".git" {
            lease.directory.join(name)
        } else {
            lease
                .origin
                .join(".git/worktrees")
                .join(lease.worktree.to_string())
                .join(name)
        };
        fs::write(path, bytes).unwrap();
        let before = lease.fixture.records();
        let printed = lease
            .cli
            .args(["ticket", "open", &lease.card.to_string(), "--print"])
            .run();
        assert_eq!(printed.code(), 4, "{}", printed.output());
        assert_eq!(printed.stdout, "");
        assert_eq!(lease.fixture.records(), before);
    }
}
