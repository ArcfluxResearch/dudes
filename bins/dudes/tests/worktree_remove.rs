//! `dudes worktree remove` against a real daemon. It records the removal of a released
//! tree whose directory is gone, says so again when it is run a second time, refuses an id the
//! daemon never leased, and treats anything left at the path, a dangling link included, as the tree
//! still being there.
#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;

use dudes::exit;
use dudes_journal::{Fsync, JournalOptions, open};
use dudes_proto::{AgentBudget, Entry, Record, WorktreeRemoval};
use dudes_types::{AgentId, GitCommit, Provider, Timestamp, WorktreeId};

mod common;

use common::{Cli, Fixture, OpenRunner, SharedHost, start};

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_000),
        record,
    }
}

fn leased(worktree: WorktreeId, path: &Path) -> Entry {
    entry(Record::WorktreeLeased {
        worktree,
        origin: path.parent().unwrap().to_path_buf(),
        path: path.to_path_buf(),
        branch: format!("dudes/{}", worktree.short()),
        base: GitCommit::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
        from: None,
    })
}

fn hired(agent: AgentId, worktree: WorktreeId, cwd: &Path) -> Entry {
    entry(Record::AgentHired {
        id: agent,
        name: "Kim".into(),
        character: None,
        accent: None,
        role: "writer".into(),
        department: "eng".into(),
        provider: Provider::Claude,
        cwd: cwd.to_path_buf(),
        workspace: None,
        budget: AgentBudget {
            usd_per_day: None,
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        model: None,
        trusted: true,
        worktree: Some(worktree),
        mcp_servers: Vec::new(),
        browser_onboarding: None,
    })
}

fn retired(agent: AgentId) -> Entry {
    entry(Record::AgentRetired {
        id: agent,
        reason: "done".into(),
        browser_retire: None,
    })
}

#[test]
fn removal_is_recorded_once_repeated_as_already_removed_and_refused_for_the_unknown() {
    let fixture = Fixture::new();
    let root = std::fs::canonicalize(fixture.state.path()).unwrap();
    let (released, removed, dangling) = (
        WorktreeId::from_bytes([1; 16]),
        WorktreeId::from_bytes([2; 16]),
        WorktreeId::from_bytes([3; 16]),
    );
    let released_path = root.join("trees/released");
    let dangling_path = root.join("trees/dangling");
    std::fs::create_dir_all(root.join("trees")).unwrap();
    std::os::unix::fs::symlink(root.join("nowhere"), &dangling_path).unwrap();
    let settings = fixture.settings();
    {
        let (mut writer, _) = open(
            &settings.state_dir.join(duded::boot::JOURNAL_DIR),
            JournalOptions {
                fsync: Fsync::None,
                ..JournalOptions::default()
            },
        )
        .unwrap();
        for step in [
            leased(released, &released_path),
            hired(AgentId::from_parts(0, 1), released, &released_path),
            retired(AgentId::from_parts(0, 1)),
            leased(removed, &root.join("trees/removed")),
            entry(Record::WorktreeRemoved {
                worktree: removed,
                how: WorktreeRemoval::NeverAssigned,
            }),
            leased(dangling, &dangling_path),
            hired(AgentId::from_parts(0, 2), dangling, &dangling_path),
            retired(AgentId::from_parts(0, 2)),
        ] {
            writer.append(&step).unwrap();
        }
    }
    let running = start(
        &settings,
        SharedHost::new(),
        Arc::new(OpenRunner::default()),
    );
    let cli = Cli::new(&settings.socket(), &settings.runtime_dir);
    let remove = |id: WorktreeId| {
        cli.clone()
            .args(["worktree", "remove", &id.to_string()])
            .run()
    };

    let unknown = remove(WorktreeId::from_bytes([9; 16]));
    assert_ne!(unknown.code(), exit::SUCCESS, "{}", unknown.output());

    let first = remove(released);
    assert_eq!(first.code(), exit::SUCCESS, "{}", first.output());
    assert!(
        first.stdout.contains("removed worktree"),
        "{}",
        first.stdout
    );
    let again = remove(released);
    assert_eq!(again.code(), exit::SUCCESS, "{}", again.output());
    assert!(again.stdout.contains("already removed"), "{}", again.stdout);
    let never_assigned = remove(removed);
    assert_eq!(
        never_assigned.code(),
        exit::SUCCESS,
        "{}",
        never_assigned.output()
    );
    assert!(never_assigned.stdout.contains("already removed"));

    let held = remove(dangling);
    assert_eq!(held.code(), exit::REFUSED, "{}", held.output());
    assert!(
        held.stderr.contains("worktree remove --"),
        "a dangling link is still something at the path: {}",
        held.stderr
    );
    std::fs::remove_file(&dangling_path).unwrap();
    let cleared = remove(dangling);
    assert_eq!(cleared.code(), exit::SUCCESS, "{}", cleared.output());
    drop(running);

    let operator_removals = fixture
        .records()
        .into_iter()
        .filter(|record| {
            matches!(
                record,
                Record::WorktreeRemoved {
                    how: WorktreeRemoval::Operator,
                    ..
                }
            )
        })
        .count();
    assert_eq!(operator_removals, 2, "each lease is recorded once");
}
