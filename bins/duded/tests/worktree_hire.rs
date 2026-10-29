//! A hire into a worktree, against real git.
//!
//! Every test boots a real daemon whose predicate seam is the real one-shot runner, so the `git`
//! children are the ones the shipped daemon runs: typed argv, a fixed environment, a deadline and a
//! process group. The hires go over the socket as the operator. No model is called.
#![cfg(unix)]

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use duded::predicate::{PredicateOutcome, PredicateRunner};
use duded::settings::Seams;
use duded::startup::Witness;
use duded::worktree::git::Git;
use duded::{ExitCause, boot};
use dudes_host::{OneShot, OneShotOutcome};
use dudes_proto::rpc::agent::{Checkout, HireParams, HireResult};
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::{ErrorCode, Method, RefusalGate, RefusedData};
use dudes_proto::{AutomaticHold, Record, WorktreeRemoval};
use dudes_providers::adapter::Adapter;
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::codex::CodexAdapter;
use dudes_providers::launch::LaunchSpec;
use dudes_providers::registry::Adapters;
use dudes_rpc::Client;
use dudes_supervisor::testing::FakeRng;
use dudes_types::{AgentId, IsolationMode, Provider, WorktreeId};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock};

struct RealRunner(OneShot);

impl PredicateRunner for RealRunner {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<PredicateOutcome> {
        self.0.run(spec, timeout).map(outcome)
    }

    fn sweep(&self) {
        self.0.sweep();
    }
}

fn outcome(outcome: OneShotOutcome) -> PredicateOutcome {
    match outcome {
        OneShotOutcome::Finished {
            exit,
            output_tail,
            timed_out,
        } => PredicateOutcome::Finished {
            exit,
            output_tail,
            timed_out,
        },
        OneShotOutcome::Swept => PredicateOutcome::Swept,
    }
}

fn runner() -> Arc<RealRunner> {
    Arc::new(RealRunner(OneShot::new()))
}

fn path_env() -> Option<std::ffi::OsString> {
    std::env::var_os("PATH")
}

/// A git over the real runner, for the tests' own setup and inspection.
fn git(_fixture: &Fixture) -> Git {
    Git::new(runner(), path_env(), Duration::from_secs(60))
}

fn run(git: &Git, cwd: &Path, args: &[&str]) -> String {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    git.exec(cwd, &args, Duration::from_secs(60))
        .unwrap_or_else(|error| panic!("git {args:?}: {error}"))
}

fn commit_all(git: &Git, dir: &Path, message: &str) {
    run(git, dir, &["add", "-A"]);
    run(
        git,
        dir,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            message,
        ],
    );
}

/// A repository with one commit, outside the daemon's state directory.
fn repo(fixture: &Fixture, git: &Git) -> PathBuf {
    let repo = std::fs::canonicalize(fixture.runtime.path())
        .unwrap()
        .join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(git, &repo, &["init", "--quiet", "-b", "main"]);
    std::fs::write(repo.join("tracked.txt"), "one\n").unwrap();
    commit_all(git, &repo, "first");
    repo
}

fn daemon(fixture: &Fixture, adapters: Adapters) -> (common::Running, duded::Settings, SharedHost) {
    daemon_with(fixture, adapters, |_| {})
}

fn daemon_with(
    fixture: &Fixture,
    adapters: Adapters,
    tweak: impl FnOnce(&mut duded::Settings),
) -> (common::Running, duded::Settings, SharedHost) {
    daemon_with_predicates(fixture, adapters, tweak, runner())
}

fn daemon_with_predicates<P: PredicateRunner + 'static>(
    fixture: &Fixture,
    adapters: Adapters,
    tweak: impl FnOnce(&mut duded::Settings),
    predicates: Arc<P>,
) -> (common::Running, duded::Settings, SharedHost) {
    let mut settings = fixture.settings();
    settings.predicate_path = path_env();
    tweak(&mut settings);
    let host = SharedHost::new();
    let daemon = boot(
        &settings,
        Seams {
            host: host.clone(),
            pty_host: Box::new(duded::ports::ClosedPtyHost),
            predicates,
            clock: Arc::new(TestClock::new()),
            rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
            resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
            adapters,
            children: duded::ChildEnds::new(),
            witness: Witness::default(),
        },
    )
    .expect("boot");
    (common::start(daemon), settings, host)
}

fn claude() -> Adapters {
    Adapters::new([Box::new(ClaudeAdapter::default()) as Box<dyn Adapter>])
}

type Socket = Client<io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream>;

fn connect(settings: &duded::Settings) -> Socket {
    Client::connect(settings.socket()).expect("connect to the daemon")
}

fn hire(
    settings: &duded::Settings,
    client: &mut Socket,
    params: &HireParams,
) -> Result<HireResult, dudes_proto::rpc::RpcError> {
    let credential = OperatorCredential::new(
        std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .expect("the daemon minted a credential"),
    )
    .expect("well formed");
    let response = client
        .call_as_operator(
            Method::AgentHire,
            Some(serde_json::to_value(params).expect("params")),
            credential,
        )
        .expect("agent.hire is answered");
    match response.error {
        Some(error) => Err(error),
        None => Ok(serde_json::from_value(response.result.expect("a result")).expect("a result")),
    }
}

fn worktree_params(repo: &Path) -> HireParams {
    let mut params = common::hire_params(repo);
    params.trusted = true;
    params.checkout = Some(Checkout::Worktree);
    params
}

fn gate(error: &dudes_proto::rpc::RpcError) -> Option<RefusalGate> {
    error
        .data
        .clone()
        .and_then(|data| serde_json::from_value::<RefusedData>(data).ok())
        .map(|data| data.gate)
}

struct Hired {
    agent: AgentId,
    worktree: WorktreeId,
    cwd: PathBuf,
}

fn hired(fixture: &Fixture, agent: AgentId) -> Hired {
    fixture
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::AgentHired {
                id,
                cwd,
                worktree: Some(worktree),
                ..
            } if id == agent => Some(Hired {
                agent,
                worktree,
                cwd,
            }),
            _ => None,
        })
        .expect("the hire recorded its worktree")
}

#[test]
fn a_worktree_hire_journals_the_lease_before_the_agent_and_works_in_its_own_tree() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let head = run(&git, &repo, &["rev-parse", "HEAD"]);
    let (mut running, settings, _) = daemon(&fixture, claude());
    let mut client = connect(&settings);

    let first = hire(&settings, &mut client, &worktree_params(&repo)).expect("a worktree hire");
    let second = hire(&settings, &mut client, &worktree_params(&repo)).expect("a second hire");
    running.stop(ExitCause::Ended);

    let (a, b) = (hired(&fixture, first.agent), hired(&fixture, second.agent));
    assert_ne!(a.cwd, b.cwd, "each agent has its own tree");
    assert_ne!(a.cwd, repo);
    for tree in [&a, &b] {
        assert!(
            tree.cwd.starts_with(
                fixture
                    .state
                    .path()
                    .canonicalize()
                    .unwrap()
                    .join("worktrees")
            )
        );
        assert_eq!(
            std::fs::read_to_string(tree.cwd.join("tracked.txt")).unwrap(),
            "one\n"
        );
    }
    let kinds: Vec<&str> = fixture
        .records()
        .iter()
        .filter_map(|record| match record {
            Record::WorktreeLeasedWithIdentity { worktree, .. } if *worktree == a.worktree => {
                Some("leased-a")
            }
            Record::AgentHired { id, .. } if *id == a.agent => Some("hired-a"),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        ["leased-a", "hired-a"],
        "the lease is journaled before the agent it is for"
    );
    let lease = fixture
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::WorktreeLeasedWithIdentity {
                worktree,
                origin,
                path,
                branch,
                base,
                from,
                ..
            } if worktree == a.worktree => Some((origin, path, branch, base, from)),
            _ => None,
        })
        .unwrap();
    assert_eq!(lease.0, repo);
    assert_eq!(lease.1, a.cwd);
    assert_eq!(lease.2, format!("dudes/{}", a.worktree.short()));
    assert_eq!(lease.3.to_string(), head);
    assert_eq!(lease.4, None);
    let branches = run(&git, &repo, &["branch", "--list", "dudes/*"]);
    assert!(branches.contains(&lease.2), "{branches}");

    std::fs::write(a.cwd.join("tracked.txt"), "changed in a\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(b.cwd.join("tracked.txt")).unwrap(),
        "one\n",
        "an edit in one tree is not in the other"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
        "one\n",
        "nor in the repository"
    );
}

#[test]
fn inherit_uses_the_owned_lease_folder_when_a_peer_is_live_in_the_origin() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let origin = repo(&fixture, &git);
    let (mut running, settings, host) = daemon(&fixture, claude());
    let mut client = connect(&settings);
    let peer = hire(&settings, &mut client, &common::hire_params(&origin))
        .unwrap()
        .agent;
    common::set_goal(
        &running.handle,
        common::goal_params(peer, &["/usr/bin/false"]),
    );
    fixture.await_record("the live origin peer", |record| match record {
        Record::TurnSpawned { agent, .. } if *agent == peer => Some(()),
        _ => None,
    });
    let mut request = worktree_params(&origin);
    request.name = "Worktree candidate".to_owned();
    request.isolation = Some(IsolationMode::Inherit);
    let candidate = hire(&settings, &mut client, &request)
        .expect("inherit in the owned lease is admitted")
        .agent;
    let tree = hired(&fixture, candidate);
    assert_ne!(
        tree.cwd.canonicalize().unwrap(),
        origin.canonicalize().unwrap()
    );
    assert!(tree.cwd.join(".git").is_file());
    common::set_goal(
        &running.handle,
        common::goal_params(candidate, &["/usr/bin/false"]),
    );
    fixture.await_record("candidate child in its lease", |record| match record {
        Record::TurnSpawned { agent, .. } if *agent == candidate => Some(()),
        _ => None,
    });
    assert_eq!(host.spawned().len(), 2);
    assert_eq!(host.spawned()[1].cwd, tree.cwd);
    assert!(fixture.records().iter().all(|record| !matches!(record,
        Record::AgentHeld { agent, cause: AutomaticHold::LaunchRefused { .. }, .. } if *agent == candidate)));
    running.stop(ExitCause::ShutdownRpc);
}

#[test]
fn a_hire_from_a_lease_sees_its_commits_and_not_its_uncommitted_files() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let (mut running, settings, _) = daemon(&fixture, claude());
    let mut client = connect(&settings);

    let first = hire(&settings, &mut client, &worktree_params(&repo)).expect("first");
    let a = hired(&fixture, first.agent);
    std::fs::write(a.cwd.join("committed.txt"), "from a\n").unwrap();
    commit_all(&git, &a.cwd, "a's work");
    std::fs::write(a.cwd.join("scratch.txt"), "not committed\n").unwrap();

    let mut from = worktree_params(&repo);
    from.from = Some(a.worktree);
    let second = hire(&settings, &mut client, &from).expect("a hire from a's lease");
    running.stop(ExitCause::Ended);

    let b = hired(&fixture, second.agent);
    assert_eq!(
        std::fs::read_to_string(b.cwd.join("committed.txt")).unwrap(),
        "from a\n"
    );
    assert!(
        !b.cwd.join("scratch.txt").exists(),
        "a's uncommitted file stays in a's tree"
    );
    let lease_from = fixture
        .records()
        .into_iter()
        .find_map(|record| match record {
            Record::WorktreeLeasedWithIdentity { worktree, from, .. } if worktree == b.worktree => {
                Some(from)
            }
            _ => None,
        });
    assert_eq!(lease_from, Some(Some(a.worktree)));
}

#[test]
fn a_refused_worktree_hire_journals_nothing_and_creates_no_tree() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let outside = fixture.runtime.path().join("not-a-repo");
    std::fs::create_dir_all(&outside).unwrap();
    let adapters = Adapters::new([
        Box::new(ClaudeAdapter::default()) as Box<dyn Adapter>,
        Box::new(CodexAdapter::default()),
    ]);
    let (mut running, settings, _) = daemon(&fixture, adapters);
    let mut client = connect(&settings);

    let mut untrusted = worktree_params(&repo);
    untrusted.trusted = false;
    let mut codex = worktree_params(&repo);
    codex.provider = Provider::Codex;
    let mut unknown_from = worktree_params(&repo);
    unknown_from.from = Some(WorktreeId::from_bytes([7; 16]));
    let mut from_alone = common::hire_params(&repo);
    from_alone.trusted = true;
    from_alone.from = Some(WorktreeId::from_bytes([7; 16]));
    let not_a_repo = worktree_params(&outside);

    for (what, params, code, expected_gate) in [
        ("untrusted", untrusted, ErrorCode::InvalidParams, None),
        ("codex", codex, ErrorCode::InvalidParams, None),
        (
            "an unknown lease",
            unknown_from,
            ErrorCode::Refused,
            Some(RefusalGate::WorktreeUnavailable),
        ),
        (
            "from without checkout",
            from_alone,
            ErrorCode::InvalidParams,
            None,
        ),
        (
            "outside a repository",
            not_a_repo,
            ErrorCode::Refused,
            Some(RefusalGate::WorktreeUnavailable),
        ),
    ] {
        let error = hire(&settings, &mut client, &params).expect_err(&format!("{what} is refused"));
        assert_eq!(error.code, code.code(), "{what}: {}", error.message);
        assert_eq!(gate(&error), expected_gate, "{what}");
    }
    running.stop(ExitCause::Ended);

    assert!(
        fixture.records().is_empty(),
        "a refusal journals no lease and no agent: {:?}",
        fixture.records()
    );
    assert!(!fixture.state.path().join("worktrees").exists());
    assert_eq!(
        run(&git, &repo, &["branch", "--list", "dudes/*"]),
        "",
        "and makes no branch"
    );
}

/// A model in the Claude configuration's extra arguments would beat a hire's model unrecorded, so
/// a worktree hire that names one is refused before a lease is journaled or a tree made.
#[test]
fn a_worktree_hire_that_names_a_model_is_refused_when_the_extra_arguments_set_one() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let claude = ClaudeConfig {
        extra_args: ["--model=haiku"].map(std::ffi::OsString::from).to_vec(),
        ..ClaudeConfig::default()
    };
    let adapters = Adapters::new([Box::new(ClaudeAdapter::new(claude)) as Box<dyn Adapter>]);
    let (mut running, settings, _) = daemon(&fixture, adapters);
    let mut client = connect(&settings);

    let mut named = worktree_params(&repo);
    named.model = Some(dudes_types::ModelName::parse("opus").unwrap());
    let error = hire(&settings, &mut client, &named).expect_err("a named model is refused");
    assert!(error.message.contains("configuration sets"), "{error:?}");
    assert!(
        fixture.records().is_empty(),
        "no lease and no agent: {:?}",
        fixture.records()
    );
    assert!(!fixture.state.path().join("worktrees").exists());
    assert_eq!(run(&git, &repo, &["branch", "--list", "dudes/*"]), "");

    hire(&settings, &mut client, &worktree_params(&repo)).expect("no model goes through");
    running.stop(ExitCause::Ended);
    assert!(
        fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::AgentHired { .. }))
    );
}

#[test]
fn a_failed_root_creation_refuses_before_a_lease_or_branch() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    std::fs::write(fixture.state.path().join("worktrees"), "blocked").unwrap();
    let (mut running, settings, _) = daemon(&fixture, claude());
    let error = hire(&settings, &mut connect(&settings), &worktree_params(&repo)).unwrap_err();
    running.stop(ExitCause::Ended);
    assert_eq!(gate(&error), Some(RefusalGate::WorktreeUnavailable));
    assert!(fixture.records().is_empty());
    assert_eq!(run(&git, &repo, &["branch", "--list", "dudes/*"]), "");
}

fn directory_identity(path: &Path) -> dudes_proto::record::WorktreeDirectoryIdentity {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).unwrap();
    let born = meta
        .created()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    dudes_proto::record::WorktreeDirectoryIdentity {
        device: meta.dev(),
        inode: meta.ino(),
        created_secs: born.as_secs(),
        created_nanos: born.subsec_nanos(),
    }
}

fn pending(fixture: &Fixture, origin: &Path, byte: u8) -> (WorktreeId, PathBuf, String) {
    let id = WorktreeId::from_bytes([byte; 16]);
    let path = fixture.state.path().join("worktrees").join(id.to_string());
    std::fs::create_dir_all(&path).unwrap();
    let identity = dudes_proto::record::WorktreeDirectoryAuthority {
        origin: directory_identity(origin),
        common: directory_identity(&origin.join(".git")),
        root: directory_identity(&path),
        parent: directory_identity(path.parent().unwrap()),
    };
    let branch = format!("dudes/{}", id.short());
    let record = Record::WorktreeLeasedWithIdentity {
        worktree: id,
        origin: origin.to_path_buf(),
        path: path.clone(),
        branch: branch.clone(),
        base: git(fixture).commit(origin, "HEAD").unwrap(),
        from: None,
        identity,
    };
    let (mut writer, _) = dudes_journal::open(
        &fixture.journal_dir(),
        dudes_journal::JournalOptions::default(),
    )
    .unwrap();
    writer
        .append(&dudes_proto::Entry {
            at: dudes_types::Timestamp::UNIX_EPOCH,
            record,
        })
        .unwrap();
    drop(writer);
    (id, path, branch)
}

fn cleanup_boot(fixture: &Fixture, id: WorktreeId) {
    let (mut running, _, _) = daemon(fixture, claude());
    fixture.await_record("pending cleanup ended", |record| match record {
        Record::WorktreeRemoved {
            worktree,
            how: WorktreeRemoval::NeverAssigned,
        } if *worktree == id => Some(()),
        _ => None,
    });
    running.stop(ExitCause::Ended);
}

#[test]
fn a_proved_empty_root_is_cleaned_after_the_required_lease_fsync() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let (id, tree, _) = pending(&fixture, &repo, 5);
    assert!(std::fs::read_dir(&tree).unwrap().next().is_none());
    cleanup_boot(&fixture, id);
    assert!(!tree.exists());
}

#[test]
fn a_definitely_absent_pending_root_is_settled_at_boot() {
    let fixture = Fixture::new();
    let repo = repo(&fixture, &git(&fixture));
    let (id, tree, _) = pending(&fixture, &repo, 6);
    std::fs::remove_dir(&tree).unwrap();
    cleanup_boot(&fixture, id);
    assert!(!tree.exists());
}

#[test]
fn a_genuine_partial_git_registration_is_cleaned_and_its_branch_is_retained() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let (id, tree, branch) = pending(&fixture, &repo, 7);
    let admin = repo.join(".git/worktrees").join(id.to_string());
    std::fs::create_dir_all(&admin).unwrap();
    let canonical_tree = std::fs::canonicalize(&tree).unwrap();
    // The pre-HEAD crash prefix has linking files and the initializing lock, with no ref store or index.
    std::fs::write(
        admin.join("gitdir"),
        format!("{}\n", canonical_tree.join(".git").display()),
    )
    .unwrap();
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    std::fs::write(admin.join("locked"), "initializing").unwrap();
    std::fs::write(tree.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
    run(&git, &repo, &["branch", &branch]);
    assert!(!admin.join("HEAD").exists() && !admin.join("index").exists());
    cleanup_boot(&fixture, id);
    assert!(!tree.exists());
    assert!(!admin.exists());
    assert_eq!(run(&git, &repo, &["branch", "--list", &branch]), branch);
}

#[test]
fn a_legacy_pending_root_is_retained_at_boot() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let (id, tree, branch) = pending(&fixture, &repo, 8);
    let record = Record::WorktreeLeased {
        worktree: WorktreeId::from_bytes([9; 16]),
        origin: repo.clone(),
        path: tree.clone(),
        branch,
        base: git.commit(&repo, "HEAD").unwrap(),
        from: None,
    };
    // The first lease owns the original, while the legacy record sees the replacement.
    let replacement = tree.with_file_name("saved-original");
    std::fs::rename(&tree, &replacement).unwrap();
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("keep"), "replacement").unwrap();
    let (mut writer, _) = dudes_journal::open(
        &fixture.journal_dir(),
        dudes_journal::JournalOptions::default(),
    )
    .unwrap();
    writer
        .append(&dudes_proto::Entry {
            at: dudes_types::Timestamp::UNIX_EPOCH,
            record,
        })
        .unwrap();
    drop(writer);
    let (sentinel, _, _) = pending(&fixture, &repo, 10);
    cleanup_boot(&fixture, sentinel);
    assert_eq!(
        std::fs::read_to_string(tree.join("keep")).unwrap(),
        "replacement"
    );
    assert!(!fixture.records().iter().any(
        |record| matches!(record, Record::WorktreeRemoved { worktree, .. } if *worktree == id)
    ));
}

#[test]
fn a_restart_reuses_the_tree_and_a_deleted_tree_holds_its_agent() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let (mut running, settings, _) = daemon(&fixture, claude());
    let mut client = connect(&settings);
    let hired_agent = hire(&settings, &mut client, &worktree_params(&repo)).expect("a hire");
    running.stop(ExitCause::Ended);
    let tree = hired(&fixture, hired_agent.agent);

    let (mut running, _, _) = daemon(&fixture, claude());
    running.stop(ExitCause::Ended);
    let held = |fixture: &Fixture| {
        fixture
            .records()
            .into_iter()
            .filter(|record| matches!(record, Record::AgentHeld { .. }))
            .count()
    };
    assert_eq!(
        held(&fixture),
        0,
        "a restart over an intact tree holds nothing"
    );

    std::fs::remove_dir_all(&tree.cwd).unwrap();
    let (mut running, _, _) = daemon(&fixture, claude());
    running.stop(ExitCause::Ended);

    let causes: Vec<AutomaticHold> = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::AgentHeld { agent, cause, .. } if agent == tree.agent => Some(cause),
            _ => None,
        })
        .collect();
    assert_eq!(
        causes,
        [AutomaticHold::WorktreeMissing {
            worktree: tree.worktree
        }]
    );

    let (mut running, _, _) = daemon(&fixture, claude());
    running.stop(ExitCause::Ended);
    assert_eq!(
        held(&fixture),
        1,
        "replay reproduces the hold and adds no second"
    );
    let _ = ScriptedRunner::default();
}

/// Journal one lease at `tree`, with an agent hired into it, before the daemon boots, so it reaches
/// the daemon by replay. A pending lease would be cleaned up at boot, which is a different test.
fn seed_active_lease(fixture: &Fixture, origin: &Path, tree: &Path) -> WorktreeId {
    let worktree = WorktreeId::from_bytes([0xa0; 16]);
    let (mut writer, _) = dudes_journal::open(
        &fixture.journal_dir(),
        dudes_journal::JournalOptions {
            fsync: dudes_journal::Fsync::None,
            ..dudes_journal::JournalOptions::default()
        },
    )
    .unwrap();
    writer
        .append(&dudes_proto::Entry {
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_600_000),
            record: Record::WorktreeLeased {
                worktree,
                origin: origin.to_path_buf(),
                path: tree.to_path_buf(),
                branch: "dudes/a0a0a0a0a0a0".into(),
                base: dudes_types::GitCommit::parse("0123456789abcdef0123456789abcdef01234567")
                    .unwrap(),
                from: None,
            },
        })
        .unwrap();
    writer
        .append(&dudes_proto::Entry {
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_600_001),
            record: Record::AgentHired {
                id: AgentId::from_parts(0, 1),
                name: "Seed".into(),
                character: None,
                accent: None,
                role: "writer".into(),
                department: "eng".into(),
                provider: Provider::Claude,
                cwd: tree.to_path_buf(),
                workspace: None,
                budget: dudes_proto::AgentBudget {
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
            },
        })
        .unwrap();
    worktree
}

fn kinds(fixture: &Fixture) -> Vec<&'static str> {
    fixture
        .records()
        .iter()
        .map(|record| match record {
            Record::WorktreeLeased { .. } | Record::WorktreeLeasedWithIdentity { .. } => "leased",
            Record::WorktreeRemoved {
                how: WorktreeRemoval::NeverAssigned,
                ..
            } => "never_assigned",
            Record::WorktreeRemoved { .. } => "removed",
            Record::AgentHired { .. } => "hired",
            _ => "other",
        })
        .collect()
}

/// On the first lease check: a repository above a live lease is refused before any git runs in it,
/// even though the directory the caller named is beside the lease.
#[test]
fn a_repository_above_a_live_lease_is_refused_before_a_lease_is_journaled() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    seed_active_lease(&fixture, &repo, &repo.join("other-tree"));
    let (wrapper, log) = logging_git(&fixture, None);
    let (mut running, settings, _) = daemon_with(&fixture, claude(), |settings| {
        settings.predicate_path = Some(format!("{}:/usr/bin:/bin", wrapper.display()).into());
    });
    let mut client = connect(&settings);

    let mut params = worktree_params(&repo);
    params.cwd = repo.join("sub");
    let error = hire(&settings, &mut client, &params).expect_err("the repository overlaps a lease");
    running.stop(ExitCause::Ended);

    assert_eq!(
        gate(&error),
        Some(RefusalGate::WorktreeOverlap),
        "{}",
        error.message
    );
    assert_eq!(
        kinds(&fixture),
        ["leased", "hired", "other"],
        "the seeded lease and agent, and the hold its missing tree earns at boot: nothing else"
    );
    assert_eq!(run(&git, &repo, &["branch", "--list", "dudes/*"]), "");
    let calls = std::fs::read_to_string(log).unwrap();
    assert!(
        !calls.contains("--verify"),
        "no commit was resolved in the repository before the refusal: {calls}"
    );
}

/// On the second lease check, when the hire is admitted again: a tree that lands under another
/// live lease is refused, and the tree that was just made is removed and its lease ended.
#[test]
fn a_new_tree_under_another_live_lease_is_refused_and_cleaned_up() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    seed_active_lease(&fixture, &repo, &fixture.state.path().join("worktrees"));
    let (mut running, settings, _) = daemon(&fixture, claude());
    let mut client = connect(&settings);

    let error = hire(&settings, &mut client, &worktree_params(&repo))
        .expect_err("the new tree is under the seeded lease");
    running.stop(ExitCause::Ended);

    assert_eq!(
        gate(&error),
        Some(RefusalGate::WorktreeOverlap),
        "{}",
        error.message
    );
    assert_eq!(
        kinds(&fixture),
        ["leased", "hired", "other", "leased", "never_assigned"]
    );
    let retained = fixture
        .records()
        .iter()
        .find_map(|record| {
            record
                .worktree_lease()
                .filter(|lease| lease.identity.is_some())
                .map(|lease| lease.branch.to_owned())
        })
        .unwrap();
    assert_eq!(run(&git, &repo, &["branch", "--list", "dudes/*"]), retained);
    let left = std::fs::read_dir(fixture.state.path().join("worktrees")).map_or(0, Iterator::count);
    assert_eq!(left, 0, "and its tree");
}

/// Hooks configured as `hook.<name>.command` run whatever `core.hooksPath` says, so a repository
/// that has any is refused. The same repository with the keys removed is the control.
#[test]
fn a_repository_that_configures_hooks_is_refused_and_one_without_is_served() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    run(
        &git,
        &repo,
        &["config", "hook.checkout-probe.command", "true"],
    );
    run(
        &git,
        &repo,
        &["config", "hook.checkout-probe.event", "post-checkout"],
    );
    let (mut running, settings, _) = daemon(&fixture, claude());
    let mut client = connect(&settings);

    let error = hire(&settings, &mut client, &worktree_params(&repo))
        .expect_err("a repository with hook.* config is refused");
    assert_eq!(gate(&error), Some(RefusalGate::WorktreeUnavailable));
    assert!(error.message.contains("hook"), "{}", error.message);
    assert!(fixture.records().is_empty());

    run(
        &git,
        &repo,
        &["config", "--remove-section", "hook.checkout-probe"],
    );
    hire(&settings, &mut client, &worktree_params(&repo)).expect("served once the keys are gone");
    running.stop(ExitCause::Ended);
}

/// A `git` that logs every call's directory and arguments, and answers `--show-toplevel` with
/// `redirect` when one is given, as a repository whose `core.worktree` points elsewhere would.
fn logging_git(fixture: &Fixture, redirect: Option<&Path>) -> (PathBuf, PathBuf) {
    let dir = fixture.runtime.path().join("logging-git");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("calls.log");
    let script = dir.join("git");
    let answer = redirect.map_or_else(String::new, |root| {
        format!(
            "for a in \"$@\"; do if [ \"$a\" = --show-toplevel ]; then echo '{}'; exit 0; fi; done\n",
            root.display()
        )
    });
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$PWD lazy=$GIT_NO_LAZY_FETCH $*\" >> '{}'\n{answer}exec /usr/bin/git \"$@\"\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    (dir, log)
}

/// `core.worktree` can make git report a root that is somewhere else. Whatever git answers, a
/// repository that does not contain the directory named is not used.
#[test]
fn a_repository_that_reports_a_root_elsewhere_is_refused_before_a_lease() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let elsewhere = fixture.runtime.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let (wrapper, log) = logging_git(&fixture, Some(&elsewhere));
    let (mut running, settings, _) = daemon_with(&fixture, claude(), |settings| {
        settings.predicate_path = Some(format!("{}:/usr/bin:/bin", wrapper.display()).into());
    });
    let mut client = connect(&settings);

    let error = hire(&settings, &mut client, &worktree_params(&repo))
        .expect_err("a redirected root is refused");
    running.stop(ExitCause::Ended);

    assert_eq!(
        gate(&error),
        Some(RefusalGate::WorktreeUnavailable),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("does not contain"),
        "{}",
        error.message
    );
    assert!(fixture.records().is_empty(), "nothing was journaled");
    let calls = std::fs::read_to_string(log).unwrap();
    assert!(
        !calls.contains(elsewhere.to_str().unwrap()),
        "git was never run in the reported root: {calls}"
    );
}

/// A lease from another repository is refused before any git runs in that repository.
#[test]
fn a_lease_from_another_repository_is_refused_before_git_runs_there() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let first = repo(&fixture, &git);
    let second = std::fs::canonicalize(fixture.runtime.path())
        .unwrap()
        .join("second");
    std::fs::create_dir_all(&second).unwrap();
    run(&git, &second, &["init", "--quiet", "-b", "main"]);
    std::fs::write(second.join("x.txt"), "x\n").unwrap();
    commit_all(&git, &second, "x");
    let (wrapper, log) = logging_git(&fixture, None);
    let (mut running, settings, _) = daemon_with(&fixture, claude(), |settings| {
        settings.predicate_path = Some(format!("{}:/usr/bin:/bin", wrapper.display()).into());
    });
    let mut client = connect(&settings);

    let in_second = hire(&settings, &mut client, &worktree_params(&second)).expect("a hire");
    let lease = hired(&fixture, in_second.agent).worktree;
    std::fs::write(&log, "").unwrap();
    let mut from_first = worktree_params(&first);
    from_first.from = Some(lease);
    let error = hire(&settings, &mut client, &from_first).expect_err("another repository");
    running.stop(ExitCause::Ended);

    assert_eq!(gate(&error), Some(RefusalGate::WorktreeUnavailable));
    assert!(
        error.message.contains("another repository"),
        "{}",
        error.message
    );
    assert_eq!(
        kinds(&fixture),
        ["leased", "hired"],
        "the refused hire journaled nothing"
    );
    let calls = std::fs::read_to_string(log).unwrap();
    assert!(
        calls
            .lines()
            .all(|line| !line.starts_with(second.to_str().unwrap())),
        "git ran in the other repository before the refusal: {calls}"
    );
    assert!(
        calls.contains(first.to_str().unwrap()),
        "a control: git ran in the first one: {calls}"
    );
}

/// A hook script planted in the repository does not run when the tree is created, and
/// the child that creates it carries the settings that stop it: no lazy fetch, a null hooks path.
#[test]
fn a_planted_checkout_hook_does_not_run_and_the_child_carries_the_guard_settings() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let marker = fixture.runtime.path().join("hook-ran");
    let hook = repo.join(".git/hooks/post-checkout");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let (wrapper, log) = logging_git(&fixture, None);
    let (mut running, settings, _) = daemon_with(&fixture, claude(), |settings| {
        settings.predicate_path = Some(format!("{}:/usr/bin:/bin", wrapper.display()).into());
    });
    let mut client = connect(&settings);

    hire(&settings, &mut client, &worktree_params(&repo)).expect("a worktree hire");
    running.stop(ExitCause::Ended);

    assert!(!marker.exists(), "the post-checkout hook ran");
    let calls = std::fs::read_to_string(log).unwrap();
    let add = calls
        .lines()
        .find(|line| line.contains(" worktree add "))
        .unwrap_or_else(|| panic!("no worktree add was logged: {calls}"));
    assert!(add.contains("lazy=1"), "{add}");
    assert!(add.contains("core.hooksPath=/dev/null"), "{add}");
    assert!(add.contains("core.fsmonitor=false"), "{add}");
    assert!(
        calls.lines().all(|line| line.contains("lazy=1")),
        "every call carries it: {calls}"
    );
}

/// An origin whose `.git` is a file (a linked worktree here; a submodule and a separate git
/// directory are the same shape) is refused before any lease, because its common directory is not
/// `<origin>/.git` and the restart check would hold the agent every boot.
#[test]
fn an_origin_that_is_a_linked_worktree_is_refused_and_nothing_is_journaled() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let linked = repo.parent().unwrap().join("linked");
    run(
        &git,
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "side",
            linked.to_str().unwrap(),
        ],
    );
    let (mut running, settings, _) = daemon(&fixture, claude());
    let mut client = connect(&settings);

    let error = hire(&settings, &mut client, &worktree_params(&linked))
        .expect_err("a linked worktree is not a plain origin");
    let control = hire(&settings, &mut client, &worktree_params(&repo))
        .expect("the plain repository itself is served");
    running.stop(ExitCause::Ended);

    assert_eq!(
        gate(&error),
        Some(RefusalGate::WorktreeUnavailable),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("plain directory"),
        "{}",
        error.message
    );
    assert_eq!(
        kinds(&fixture),
        ["leased", "hired"],
        "only the control hire was journaled"
    );
    let _ = control;
}

fn call(
    settings: &duded::Settings,
    client: &mut Socket,
    method: Method,
    params: Option<serde_json::Value>,
) -> Result<serde_json::Value, dudes_proto::rpc::RpcError> {
    let credential = OperatorCredential::new(
        std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .expect("the daemon minted a credential"),
    )
    .expect("well formed");
    let response = client
        .call_as_operator(method, params, credential)
        .expect("the call is answered");
    match response.error {
        Some(error) => Err(error),
        None => Ok(response.result.unwrap_or(serde_json::Value::Null)),
    }
}

/// The operator lists trees with their paths, and records a removal only for a
/// released tree whose directory is gone. The Floor's agent rows name the branch and the state and
/// never the path, and two agents in one ordinary directory are shown as sharing it.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one story: list, hide the path, refuse, retire, remove and record"
)]
fn the_operator_lists_and_removes_a_released_tree_and_the_floor_never_sees_its_path() {
    use dudes_proto::rpc::worktree::{RemovedParams, WorktreeEntry};
    use dudes_proto::{AgentView, WorktreeViewState};
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let (mut running, settings, _) = daemon(&fixture, claude());
    let mut client = connect(&settings);

    let hired_agent = hire(&settings, &mut client, &worktree_params(&repo)).expect("a hire");
    let mut shared = common::hire_params(&repo);
    shared.name = "Kim".to_owned();
    let (first_plain, second_plain) = (
        hire(&settings, &mut client, &shared).expect("an ordinary hire"),
        hire(&settings, &mut client, &shared).expect("a second ordinary hire"),
    );
    let tree = hired(&fixture, hired_agent.agent);

    let list = |client: &mut Socket| -> Vec<WorktreeEntry> {
        serde_json::from_value(call(&settings, client, Method::WorktreeList, None).unwrap())
            .unwrap()
    };
    let entries = list(&mut client);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        (entries[0].worktree, entries[0].agent, entries[0].state),
        (
            tree.worktree,
            Some(hired_agent.agent),
            WorktreeViewState::Active
        )
    );
    assert_eq!(entries[0].path, tree.cwd);
    assert_eq!(entries[0].origin, repo);

    let agents_json = call(&settings, &mut client, Method::AgentList, None).unwrap();
    let text = agents_json.to_string();
    assert!(
        !text.contains(tree.cwd.to_str().unwrap()) && !text.contains(repo.to_str().unwrap()),
        "no path reaches an agent row: {text}"
    );
    let agents: Vec<AgentView> = serde_json::from_value(agents_json).unwrap();
    let row = |id: AgentId| agents.iter().find(|agent| agent.id == id).unwrap();
    let view = row(hired_agent.agent)
        .worktree
        .clone()
        .expect("a worktree view");
    assert_eq!(view.branch, format!("dudes/{}", tree.worktree.short()));
    assert_eq!(view.state, WorktreeViewState::Active);
    assert!(!row(hired_agent.agent).shares_checkout);
    assert!(row(first_plain.agent).shares_checkout && row(second_plain.agent).shares_checkout);
    assert!(row(first_plain.agent).worktree.is_none());

    let removed = |client: &mut Socket| {
        call(
            &settings,
            client,
            Method::WorktreeRemoved,
            Some(
                serde_json::to_value(RemovedParams {
                    worktree: tree.worktree,
                })
                .unwrap(),
            ),
        )
    };
    let refusal = removed(&mut client).expect_err("an active tree cannot be recorded removed");
    assert_eq!(gate(&refusal), Some(RefusalGate::WorktreeUnavailable));

    call(
        &settings,
        &mut client,
        Method::AgentRetire,
        Some(
            serde_json::to_value(dudes_proto::rpc::agent::RetireParams {
                agent: hired_agent.agent,
                reason: "done".to_owned(),
            })
            .unwrap(),
        ),
    )
    .expect("retire");
    assert_eq!(list(&mut client)[0].state, WorktreeViewState::Released);
    assert!(
        removed(&mut client)
            .expect_err("the tree still exists")
            .message
            .contains("still exists")
    );

    run(
        &git,
        &repo,
        &["worktree", "remove", "--", tree.cwd.to_str().unwrap()],
    );
    removed(&mut client).expect("a released tree whose directory is gone is recorded removed");
    removed(&mut client).expect("recording it again changes nothing");
    assert!(list(&mut client).is_empty());
    running.stop(ExitCause::Ended);

    let how: Vec<WorktreeRemoval> = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::WorktreeRemoved { how, .. } => Some(how),
            _ => None,
        })
        .collect();
    assert_eq!(how, [WorktreeRemoval::Operator], "recorded once");
    assert_eq!(
        run(&git, &repo, &["branch", "--list", "dudes/*"]),
        format!("dudes/{}", tree.worktree.short()),
        "the branch is kept"
    );
}

/// A path that holds a dangling link is not a path that is gone: `exists()` follows the link and
/// says it is, which would let the record be written over something still there.
#[test]
fn a_dangling_link_where_the_tree_was_is_not_a_removed_tree() {
    use dudes_proto::rpc::worktree::RemovedParams;
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let (mut running, settings, _) = daemon(&fixture, claude());
    let mut client = connect(&settings);
    let agent = hire(&settings, &mut client, &worktree_params(&repo)).expect("a hire");
    let tree = hired(&fixture, agent.agent);
    call(
        &settings,
        &mut client,
        Method::AgentRetire,
        Some(
            serde_json::to_value(dudes_proto::rpc::agent::RetireParams {
                agent: agent.agent,
                reason: "done".to_owned(),
            })
            .unwrap(),
        ),
    )
    .expect("retire");
    run(
        &git,
        &repo,
        &[
            "worktree",
            "remove",
            "--force",
            "--",
            tree.cwd.to_str().unwrap(),
        ],
    );
    std::os::unix::fs::symlink(fixture.runtime.path().join("nowhere"), &tree.cwd).unwrap();
    let removed = |client: &mut Socket| {
        call(
            &settings,
            client,
            Method::WorktreeRemoved,
            Some(
                serde_json::to_value(RemovedParams {
                    worktree: tree.worktree,
                })
                .unwrap(),
            ),
        )
    };

    let refusal = removed(&mut client).expect_err("something is still at the path");
    assert!(
        refusal.message.contains("still exists"),
        "{}",
        refusal.message
    );
    std::fs::remove_file(&tree.cwd).unwrap();
    removed(&mut client).expect("recorded once the path holds nothing");
    running.stop(ExitCause::Ended);
}

#[test]
fn a_crash_between_two_copies_cleans_both_and_retry_uses_fresh_leases() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let (first, first_root, first_branch) = pending(&fixture, &repo, 11);
    git.add(
        &repo,
        &first_root,
        &first_branch,
        git.commit(&repo, "HEAD").unwrap(),
    )
    .unwrap();
    let (second, second_root, _) = pending(&fixture, &repo, 12);
    let (mut running, settings, _) = daemon(&fixture, claude());
    for pending in [first, second] {
        fixture.await_record("both pending copies settle", |record| match record {
            Record::WorktreeRemoved {
                worktree,
                how: WorktreeRemoval::NeverAssigned,
            } if *worktree == pending => Some(()),
            _ => None,
        });
    }
    assert!(!first_root.exists());
    assert!(!second_root.exists());
    let retried = hire(&settings, &mut connect(&settings), &worktree_params(&repo)).unwrap();
    running.stop(ExitCause::Ended);
    let fresh = hired(&fixture, retried.agent);
    assert!(![first, second].contains(&fresh.worktree));
    assert!(fresh.cwd.join("tracked.txt").is_file());
    assert!(!fixture.records().iter().any(|record| matches!(
        record,
        Record::TurnIssued { .. } | Record::TurnSpawned { .. }
    )));
}

#[test]
fn an_advanced_pending_branch_and_its_copy_are_retained_at_boot() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let (id, tree, branch) = pending(&fixture, &repo, 13);
    git.add(&repo, &tree, &branch, git.commit(&repo, "HEAD").unwrap())
        .unwrap();
    std::fs::write(tree.join("new.txt"), "keep committed work").unwrap();
    commit_all(&git, &tree, "later work");
    let advanced = git.commit(&repo, &format!("refs/heads/{branch}")).unwrap();
    let (sentinel, _, _) = pending(&fixture, &repo, 14);
    cleanup_boot(&fixture, sentinel);
    assert_eq!(
        git.commit(&repo, &format!("refs/heads/{branch}")).unwrap(),
        advanced
    );
    assert_eq!(
        std::fs::read_to_string(tree.join("new.txt")).unwrap(),
        "keep committed work"
    );
    assert!(!fixture.records().iter().any(
        |record| matches!(record, Record::WorktreeRemoved { worktree, .. } if *worktree == id)
    ));
}

struct PartialRegistrationFailure {
    real: RealRunner,
    journal: PathBuf,
    replace: bool,
    observed: std::sync::atomic::AtomicBool,
}

impl PredicateRunner for PartialRegistrationFailure {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<PredicateOutcome> {
        if spec
            .argv
            .windows(2)
            .any(|args| args[0] == "worktree" && args[1] == "add")
        {
            let reader = dudes_journal::Reader::open(&self.journal).unwrap().0;
            let record = reader
                .iter_from(dudes_types::Lsn::ZERO)
                .unwrap()
                .map(Result::unwrap)
                .map(|(_, entry)| entry.record)
                .last()
                .unwrap();
            assert!(matches!(record, Record::WorktreeLeasedWithIdentity { .. }));
            let lease = record.worktree_lease().unwrap();
            assert!(std::fs::read_dir(lease.path).unwrap().next().is_none());
            assert_eq!(directory_identity(lease.path), lease.identity.unwrap().root);
            assert!(
                !reader
                    .iter_from(dudes_types::Lsn::ZERO)
                    .unwrap()
                    .map(Result::unwrap)
                    .any(|(_, entry)| matches!(
                        entry.record,
                        Record::AgentHired { .. }
                            | Record::TurnIssued { .. }
                            | Record::TurnSpawned { .. }
                    ))
            );
            let admin = lease
                .origin
                .join(".git/worktrees")
                .join(lease.path.file_name().unwrap());
            std::fs::create_dir_all(&admin).unwrap();
            std::fs::write(
                admin.join("gitdir"),
                format!(
                    "{}\n",
                    std::fs::canonicalize(lease.path)
                        .unwrap()
                        .join(".git")
                        .display()
                ),
            )
            .unwrap();
            std::fs::write(admin.join("commondir"), "../..\n").unwrap();
            std::fs::write(admin.join("locked"), "initializing").unwrap();
            std::fs::write(
                lease.path.join(".git"),
                format!("gitdir: {}\n", admin.display()),
            )
            .unwrap();
            let real_git = Git::new(runner(), path_env(), Duration::from_secs(1));
            run(
                &real_git,
                lease.origin,
                &["branch", lease.branch, &lease.base.to_string()],
            );
            assert!(!admin.join("HEAD").exists() && !admin.join("index").exists());
            if self.replace {
                std::fs::rename(lease.path, lease.path.with_file_name("original-retained"))
                    .unwrap();
                std::fs::create_dir(lease.path).unwrap();
                std::fs::write(
                    lease.path.join(".git"),
                    format!("gitdir: {}\n", admin.display()),
                )
                .unwrap();
                std::fs::write(lease.path.join("keep"), "replacement").unwrap();
            }
            self.observed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            return Ok(PredicateOutcome::Finished {
                exit: Some(1),
                output_tail: "interrupted before checkout".into(),
                timed_out: false,
            });
        }
        self.real.run(spec, timeout)
    }
    fn sweep(&self) {
        self.real.sweep();
    }
}

#[test]
fn failure_after_constructed_partial_registration_cleans_only_the_proved_copy() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let interrupted = Arc::new(PartialRegistrationFailure {
        real: RealRunner(OneShot::new()),
        journal: fixture.journal_dir(),
        replace: false,
        observed: std::sync::atomic::AtomicBool::new(false),
    });
    let (mut running, settings, _) =
        daemon_with_predicates(&fixture, claude(), |_| {}, Arc::clone(&interrupted));
    hire(&settings, &mut connect(&settings), &worktree_params(&repo)).unwrap_err();
    running.stop(ExitCause::Ended);
    assert!(
        interrupted
            .observed
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    let records = fixture.records();
    assert!(matches!(
        records.as_slice(),
        [
            Record::WorktreeLeasedWithIdentity { .. },
            Record::WorktreeRemoved {
                how: WorktreeRemoval::NeverAssigned,
                ..
            }
        ]
    ));
    let lease = records[0].worktree_lease().unwrap();
    assert!(!lease.path.exists());
    assert_eq!(
        run(&git, &repo, &["branch", "--list", lease.branch]),
        lease.branch
    );
}

#[test]
fn a_plausible_replacement_survives_failed_add_and_daemon_restart() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let interrupted = Arc::new(PartialRegistrationFailure {
        real: RealRunner(OneShot::new()),
        journal: fixture.journal_dir(),
        replace: true,
        observed: std::sync::atomic::AtomicBool::new(false),
    });
    let (mut running, settings, _) =
        daemon_with_predicates(&fixture, claude(), |_| {}, interrupted);
    hire(&settings, &mut connect(&settings), &worktree_params(&repo)).unwrap_err();
    running.stop(ExitCause::Ended);
    let records = fixture.records();
    assert_eq!(records.len(), 1);
    let lease = records[0].worktree_lease().unwrap();
    assert_eq!(
        std::fs::read_to_string(lease.path.join("keep")).unwrap(),
        "replacement"
    );
    let (sentinel, _, _) = pending(&fixture, &repo, 15);
    cleanup_boot(&fixture, sentinel);
    assert_eq!(
        std::fs::read_to_string(lease.path.join("keep")).unwrap(),
        "replacement"
    );
    assert!(!fixture.records().iter().any(|record| matches!(record, Record::WorktreeRemoved { worktree, .. } if *worktree == lease.worktree)));
}

#[test]
fn a_pending_copy_moved_for_cleanup_recovers_at_boot_from_its_durable_lease() {
    let fixture = Fixture::new();
    let git = git(&fixture);
    let repo = repo(&fixture, &git);
    let (id, tree, branch) = pending(&fixture, &repo, 16);
    git.add(&repo, &tree, &branch, git.commit(&repo, "HEAD").unwrap())
        .unwrap();
    let holding = tree
        .parent()
        .unwrap()
        .join(format!(".pending-cleanup-{id}"));
    std::fs::rename(&tree, &holding).unwrap();
    std::fs::File::open(tree.parent().unwrap())
        .unwrap()
        .sync_all()
        .unwrap();
    cleanup_boot(&fixture, id);
    assert!(!tree.exists());
    assert!(!holding.exists());
    assert!(!repo.join(".git/worktrees").join(id.to_string()).exists());
    assert_eq!(run(&git, &repo, &["branch", "--list", &branch]), branch);
}
