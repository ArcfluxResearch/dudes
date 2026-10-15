//! A hire whose working directory is at, under or above a worktree that is pending
//! or active is refused before anything is appended, and a released or unrelated directory is
//! admitted.
//!
//! The leases are journaled before the daemon boots and reach the daemon the way every state does,
//! by replay. The hires go over the socket as the operator, so the refusal is the one a caller
//! sees: the finite `worktree_overlap` gate on a `Refused` error.
#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_journal::{Fsync, JournalOptions, open};
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::{ErrorCode, Method, RefusalGate, RefusedData};
use dudes_proto::{AgentBudget, Entry, Record};
use dudes_rpc::Client;
use dudes_types::{AgentId, GitCommit, Provider, Timestamp, Usd, WorktreeId};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

const BASE: &str = "0123456789abcdef0123456789abcdef01234567";

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
        base: GitCommit::parse(BASE).unwrap(),
        from: None,
    })
}

fn hired_into(agent: AgentId, worktree: WorktreeId, cwd: &Path) -> Entry {
    entry(Record::AgentHired {
        id: agent,
        name: "Kim".into(),
        character: None,
        accent: None,
        role: "engineer".into(),
        department: "engineering".into(),
        provider: Provider::Claude,
        cwd: cwd.to_path_buf(),
        workspace: None,
        budget: AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
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

struct Layout {
    /// A tree leased with no agent yet.
    pending: PathBuf,
    /// A tree whose agent is hired and live.
    active: PathBuf,
    /// A tree whose agent has retired and that nothing has removed.
    released: PathBuf,
    /// A tree that was removed.
    removed: PathBuf,
    /// An ordinary directory beside them.
    elsewhere: PathBuf,
}

fn layout(fixture: &Fixture) -> Layout {
    let root = fs::canonicalize(fixture.state.path()).unwrap();
    let trees = root.join("trees");
    let layout = Layout {
        pending: trees.join("pending"),
        active: trees.join("active"),
        released: trees.join("released"),
        removed: trees.join("removed"),
        elsewhere: root.join("elsewhere"),
    };
    for dir in [
        &layout.pending,
        &layout.active,
        &layout.released,
        &layout.removed,
        &layout.elsewhere,
    ] {
        fs::create_dir_all(dir.join("src")).unwrap();
    }
    layout
}

fn seed_journal(fixture: &Fixture, layout: &Layout) {
    let (mut writer, _) = open(
        &fixture.journal_dir(),
        JournalOptions {
            fsync: Fsync::None,
            ..JournalOptions::default()
        },
    )
    .unwrap();
    // `FakeRng` ids carry timestamp 0 and a small counter, and the daemon answers a hire with the
    // newest id, so the seeded agents sort below every id this daemon will mint.
    let live = AgentId::from_parts(0, 1);
    let gone = AgentId::from_parts(0, 2);
    for step in [
        leased(WorktreeId::from_bytes([1; 16]), &layout.pending),
        leased(WorktreeId::from_bytes([2; 16]), &layout.active),
        hired_into(live, WorktreeId::from_bytes([2; 16]), &layout.active),
        leased(WorktreeId::from_bytes([3; 16]), &layout.released),
        hired_into(gone, WorktreeId::from_bytes([3; 16]), &layout.released),
        retired(gone),
        leased(WorktreeId::from_bytes([4; 16]), &layout.removed),
        entry(Record::WorktreeRemoved {
            worktree: WorktreeId::from_bytes([4; 16]),
            how: dudes_proto::WorktreeRemoval::NeverAssigned,
        }),
    ] {
        writer.append(&step).unwrap();
    }
}

fn hired_agents(fixture: &Fixture) -> usize {
    fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::AgentHired { .. }))
        .count()
}

#[test]
fn a_hire_at_under_or_above_a_live_tree_is_refused_and_appends_nothing() {
    let fixture = Fixture::new();
    let layout = layout(&fixture);
    let alias = layout.elsewhere.join("alias");
    symlink(&layout.active, &alias).unwrap();
    seed_journal(&fixture, &layout);
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let mut client: Client<
        io::BufReader<std::os::unix::net::UnixStream>,
        std::os::unix::net::UnixStream,
    > = Client::connect(settings.socket()).expect("connect to the daemon");
    let credential = || {
        OperatorCredential::new(
            fs::read_to_string(
                settings
                    .runtime_dir
                    .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
            )
            .expect("the daemon minted a credential"),
        )
        .expect("it is well formed")
    };
    let mut hire = |cwd: &Path| {
        let params = serde_json::to_value(common::hire_params(cwd)).expect("params");
        client
            .call_as_operator(Method::AgentHire, Some(params), credential())
            .expect("agent.hire is answered")
    };

    let seeded = hired_agents(&fixture);
    assert_eq!(seeded, 2, "a control: replay holds the two seeded agents");
    let trees = layout.pending.parent().unwrap();
    for (what, cwd) in [
        ("at a pending tree", layout.pending.clone()),
        ("under a pending tree", layout.pending.join("src")),
        (
            "under a pending tree, not created",
            layout.pending.join("src/new"),
        ),
        ("at an active tree", layout.active.clone()),
        ("under an active tree", layout.active.join("src")),
        ("above the trees", trees.to_path_buf()),
        (
            "above the trees, far",
            trees.parent().unwrap().to_path_buf(),
        ),
        ("through a link to an active tree", alias.clone()),
        ("under a link to an active tree", alias.join("src")),
        ("at a released tree", layout.released.clone()),
        ("under a released tree", layout.released.join("src")),
    ] {
        let response = hire(&cwd);
        let error = response
            .error
            .unwrap_or_else(|| panic!("a hire {what} is refused"));
        assert_eq!(error.code, ErrorCode::Refused.code(), "{what}");
        let data: RefusedData =
            serde_json::from_value(error.data.expect("the refusal names its gate"))
                .expect("a refusal's data");
        assert_eq!(data.gate, RefusalGate::WorktreeOverlap, "{what}");
        assert!(
            error.message.contains("worktree"),
            "{what}: {}",
            error.message
        );
    }
    assert_eq!(hired_agents(&fixture), seeded, "a refusal appends nothing");

    for (what, cwd) in [
        ("an unrelated directory", layout.elsewhere.clone()),
        ("a removed tree", layout.removed.clone()),
        ("under a removed tree", layout.removed.join("src")),
    ] {
        let response = hire(&cwd);
        assert!(response.error.is_none(), "{what}: {:?}", response.error);
    }
    running.stop(ExitCause::Ended);
    assert_eq!(
        hired_agents(&fixture),
        seeded + 3,
        "the three admitted hires reached the journal"
    );
}
