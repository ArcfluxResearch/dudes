//! Restricted repository history is readable while provider execution remains unavailable.
#![cfg(unix)]

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use duded::predicate::{PredicateOutcome, PredicateRunner};
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_host::{OneShot, OneShotOutcome};
use dudes_journal::{Projections, Reader, Writer, replay};
use dudes_proto::record::{FileIdentity, RecordedWorkspace, samples};
use dudes_proto::repository::{
    CatalogRepository, ExecutionRepository, HireRepositories, OwnedRepository, RepoId,
    RepositoryEntry, RepositoryHire, RepositoryPolicy, RepositorySet, ScopedHireRepositories,
    ScopedRepositories,
};
use dudes_proto::{Entry, Record, RecordKind, WorktreeRemoval};
use dudes_rpc::Client;
use dudes_types::{AgentId, GitCommit, Lsn, Timestamp, WorktreeId};
use serde_json::json;

mod common;
#[path = "repository_replay/current_enrollment.rs"]
mod current_enrollment;
#[path = "repository_replay/previous_reader.rs"]
mod previous_reader;
use common::{Fixture, SharedHost, TestClock};

struct RealRunner(OneShot);

impl PredicateRunner for RealRunner {
    fn run(
        &self,
        spec: &dudes_providers::launch::LaunchSpec,
        timeout: Duration,
    ) -> io::Result<PredicateOutcome> {
        self.0.run(spec, timeout).map(|outcome| match outcome {
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
        })
    }

    fn sweep(&self) {
        self.0.sweep();
    }

    fn run_captured(
        &self,
        spec: &dudes_providers::launch::LaunchSpec,
        timeout: Duration,
        stdout_cap: usize,
    ) -> io::Result<duded::predicate::PredicateCaptured> {
        self.0
            .run_captured(spec, timeout, stdout_cap)
            .map(|outcome| match outcome {
                dudes_host::CapturedOutcome::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                } => duded::predicate::PredicateCaptured::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                },
                dudes_host::CapturedOutcome::Swept => duded::predicate::PredicateCaptured::Swept,
            })
    }
}

/// How long a real git or check process may run before a test gives up on it. It only catches a
/// hang: a loaded runner starves a child for seconds, and the tests finish in well under one.
const PROCESS_BOUND: Duration = Duration::from_secs(10);

fn git_path() -> Option<OsString> {
    static GIT_PATH: OnceLock<Option<OsString>> = OnceLock::new();
    GIT_PATH.get_or_init(|| std::env::var_os("PATH")).clone()
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let git = duded::worktree::git::Git::new(
        Arc::new(RealRunner(OneShot::new())),
        git_path(),
        PROCESS_BOUND,
    );
    let args: Vec<_> = args.iter().map(std::ffi::OsStr::new).collect();
    git.exec(cwd, &args, PROCESS_BOUND).unwrap()
}

fn identity(path: &Path) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).unwrap();
    FileIdentity {
        device: meta.dev(),
        inode: meta.ino(),
    }
}

fn append(writer: &mut Writer, record: Record) -> Lsn {
    writer
        .append(&Entry {
            at: Timestamp::from_unix_millis(1_000),
            record,
        })
        .unwrap()
}

fn create_origin(root: &Path, name: &str) -> RepositoryEntry<CatalogRepository> {
    let origin = root.join(name);
    std::fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "--quiet", "-b", "main"]);
    std::fs::write(origin.join("tracked.txt"), "initial\n").unwrap();
    git(&origin, &["add", "tracked.txt"]);
    git(
        &origin,
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
            "initial",
        ],
    );
    let common = origin.join(".git");
    RepositoryEntry {
        repo_id: RepoId::parse(name).unwrap(),
        value: CatalogRepository {
            label: name.into(),
            origin: dudes_proto::record::TrustedRepository {
                directory_identity: identity(&origin),
                common_identity: identity(&common),
                directory: origin,
                common_dir: common,
            },
        },
    }
}

#[derive(Clone, Copy)]
enum LeaseCarrier {
    Legacy,
    WithIdentity,
}

fn cleanup_identity(path: &Path) -> dudes_proto::record::WorktreeDirectoryIdentity {
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

fn create_copy(
    writer: &mut Writer,
    entry: &RepositoryEntry<CatalogRepository>,
    state: &Path,
    index: usize,
    trust_lsn: Lsn,
    carrier: LeaseCarrier,
) -> OwnedRepository {
    let worktree = WorktreeId::from_bytes([u8::try_from(index + 1).unwrap(); 16]);
    let path = state.join("worktrees").join(worktree.to_string());
    let origin = &entry.value.origin;
    let branch = match carrier {
        LeaseCarrier::Legacy => format!("dudes/{}-{worktree}", entry.repo_id.as_str()),
        LeaseCarrier::WithIdentity => format!("dudes/{}", worktree.short()),
    };
    let base = GitCommit::parse(&git(&origin.directory, &["rev-parse", "HEAD"])).unwrap();
    let lease = match carrier {
        LeaseCarrier::Legacy => Record::WorktreeLeased {
            worktree,
            origin: origin.directory.clone(),
            path: path.clone(),
            branch: branch.clone(),
            base,
            from: None,
        },
        LeaseCarrier::WithIdentity => {
            use std::os::unix::fs::DirBuilderExt;
            let parent = path.parent().unwrap();
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .unwrap();
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            std::fs::File::open(&path).unwrap().sync_all().unwrap();
            std::fs::File::open(parent).unwrap().sync_all().unwrap();
            let authority = dudes_proto::record::WorktreeDirectoryAuthority {
                origin: cleanup_identity(&origin.directory),
                common: cleanup_identity(&origin.common_dir),
                root: cleanup_identity(&path),
                parent: cleanup_identity(parent),
            };
            Record::WorktreeLeasedWithIdentity {
                worktree,
                origin: origin.directory.clone(),
                path: path.clone(),
                branch: branch.clone(),
                base,
                from: None,
                identity: authority,
            }
        }
    };
    append(writer, lease);
    git(
        &origin.directory,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            &branch,
            path.to_str().unwrap(),
            &base.to_string(),
        ],
    );
    let git_dir = PathBuf::from(git(&path, &["rev-parse", "--absolute-git-dir"]));
    OwnedRepository {
        origin: origin.clone(),
        execution: ExecutionRepository {
            directory_identity: identity(&path),
            git_identity: identity(&git_dir),
            directory: path,
            git_dir,
        },
        worktree,
        base,
        trust_lsn,
    }
}

struct Story {
    hire: RepositoryHire,
    hired_lsn: Lsn,
}

impl Story {
    fn seed(fixture: &Fixture) -> Self {
        let root = std::fs::canonicalize(fixture.runtime.path()).unwrap();
        let state = std::fs::canonicalize(fixture.state.path()).unwrap();
        let catalog = ["web", "api", "docs"]
            .into_iter()
            .map(|name| create_origin(&root, name))
            .collect();
        let catalog = RepositorySet::new(RepoId::parse("api").unwrap(), catalog).unwrap();
        let (mut writer, _) =
            dudes_journal::open(&fixture.journal_dir(), fixture.settings().journal).unwrap();
        let catalog_lsn = append(
            &mut writer,
            Record::ProjectRepositoriesRecorded {
                project: "service".into(),
                repositories: catalog.clone(),
            },
        );
        let Record::WorkspaceTrustSet { pin, .. } = samples::record(RecordKind::WorkspaceTrustSet)
        else {
            unreachable!()
        };
        let mut grants = std::collections::BTreeMap::new();
        for entry in catalog.entries() {
            let lsn = append(
                &mut writer,
                Record::RepositoryTrustSet {
                    project: "service".into(),
                    repo_id: entry.repo_id.clone(),
                    catalog_lsn,
                    origin: entry.value.origin.clone(),
                    pin: pin.clone(),
                },
            );
            grants.insert(entry.repo_id.clone(), lsn);
        }
        let mut lease_order: Vec<_> = catalog.entries().iter().collect();
        lease_order.sort_by_key(|entry| &entry.repo_id);
        let mut copies = std::collections::BTreeMap::new();
        for (index, entry) in lease_order.into_iter().enumerate() {
            copies.insert(
                entry.repo_id.clone(),
                create_copy(
                    &mut writer,
                    entry,
                    &state,
                    index,
                    grants[&entry.repo_id],
                    LeaseCarrier::Legacy,
                ),
            );
        }
        let repositories = RepositorySet::new(
            catalog.primary_id().clone(),
            catalog
                .entries()
                .iter()
                .map(|entry| RepositoryEntry {
                    repo_id: entry.repo_id.clone(),
                    value: copies[&entry.repo_id].clone(),
                })
                .collect(),
        )
        .unwrap();
        let hire = RepositoryHire {
            id: AgentId::from_parts(3, 1),
            name: "Kim".into(),
            role: "engineer".into(),
            department: "eng".into(),
            character: None,
            accent: None,
            provider: dudes_types::Provider::Claude,
            budget: dudes_proto::AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            max_turns: None,
            max_budget: None,
            model: None,
            isolation: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
            repositories: ScopedRepositories {
                project: RecordedWorkspace {
                    id: "service".into(),
                    label: "Service".into(),
                },
                catalog_lsn,
                policy: RepositoryPolicy::WorkingTreeV1,
                repositories,
            },
        };
        let hired_lsn = append(
            &mut writer,
            Record::AgentHiredWithRepositories(Box::new(hire.clone())),
        );
        Self { hire, hired_lsn }
    }

    fn copies(&self) -> impl Iterator<Item = &OwnedRepository> {
        self.hire
            .repositories
            .repositories
            .entries()
            .iter()
            .map(|entry| &entry.value)
    }
}

fn settings(fixture: &Fixture) -> duded::Settings {
    let mut settings = fixture.settings();
    settings.predicate_path = git_path();
    settings
}

fn scoped_hire(world: &dudes_supervisor::World, agent: AgentId) -> &ScopedHireRepositories {
    let HireRepositories::Scoped(hire) = &world.agents[&agent].repositories else {
        panic!("scoped hire")
    };
    hire
}

fn start(fixture: &Fixture, host: &SharedHost) -> (common::Running, duded::Settings) {
    let settings = settings(fixture);
    let daemon = boot(
        &settings,
        common::seams(
            &settings,
            host.clone(),
            Arc::new(RealRunner(OneShot::new())),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    (common::start(daemon), settings)
}

fn credential(settings: &duded::Settings) -> dudes_proto::rpc::cred::OperatorCredential {
    dudes_proto::rpc::cred::OperatorCredential::new(
        std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn all_committed_copies_survive_boot_and_have_identical_live_warm_and_rebuilt_authority() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let settings = settings(&fixture);
    let host = SharedHost::new();
    let daemon = boot(
        &settings,
        common::seams(
            &settings,
            host.clone(),
            Arc::new(RealRunner(OneShot::new())),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let agent = &daemon.world().agents[&story.hire.id];
    let expected_hire = agent.repositories.clone();
    assert_eq!(
        scoped_hire(daemon.world(), story.hire.id).recorded(),
        story.hire.repositories
    );
    assert_eq!(agent.repositories.worktrees().count(), 3);
    assert!(
        story
            .copies()
            .all(|repo| daemon.world().worktrees[&repo.worktree].agent == Some(story.hire.id))
    );
    assert!(
        story
            .copies()
            .all(|repo| daemon.world().worktree_state(repo.worktree)
                == Some(dudes_supervisor::WorktreeState::Active))
    );
    let mut running = common::start(daemon);
    let response = Client::connect(settings.socket())
        .unwrap()
        .call_as_operator(
            dudes_proto::rpc::Method::AgentList,
            Some(json!({})),
            credential(&settings),
        )
        .unwrap();
    assert!(response.error.is_none());
    let attached = Client::connect(settings.socket())
        .unwrap()
        .call_as_operator(
            dudes_proto::rpc::Method::AgentAttach,
            Some(json!({"agent": story.hire.id, "cols": 80, "rows": 24})),
            credential(&settings),
        )
        .unwrap();
    assert!(
        attached.error.is_some(),
        "restricted attached execution is unavailable"
    );
    running.stop(ExitCause::Ended);
    assert!(host.spawned().is_empty());
    assert!(story.copies().all(|repo| repo.execution.directory.is_dir()));
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::WorktreeRemoved { .. })
                || record.turn_issue().is_some())
    );

    let reader = Reader::open(&fixture.journal_dir()).unwrap().0;
    let cache = settings.state_dir.join(duded::boot::PROJECTION_FILE);
    let mut warm = Projections::open(&cache).unwrap();
    assert_eq!(replay(&reader, &mut warm).unwrap().applied, 0);
    let expected = warm.export().unwrap();
    let row = warm.agent(story.hire.id).unwrap().unwrap();
    assert_eq!(row.hired_lsn, story.hired_lsn);
    assert_eq!(row.repositories, expected_hire);
    assert_eq!(warm.repository_projects().unwrap()[0].revisions.len(), 1);
    assert_eq!(warm.repository_trust().unwrap().len(), 3);
    assert!(
        warm.worktrees()
            .unwrap()
            .iter()
            .all(|row| row.agent == Some(story.hire.id))
    );
    drop(warm);
    let mut rebuilt = Projections::open(&fixture.runtime.path().join("rebuilt.redb")).unwrap();
    assert!(replay(&reader, &mut rebuilt).unwrap().applied > 0);
    assert_eq!(rebuilt.export().unwrap(), expected);
    drop(rebuilt);
    std::fs::remove_file(cache).unwrap();
    let (mut running, _) = start(&fixture, &host);
    running.stop(ExitCause::Ended);
    assert!(host.spawned().is_empty());
    assert!(story.copies().all(|repo| repo.execution.directory.is_dir()));
}

#[test]
fn retiring_a_plural_hire_releases_every_copy_without_removing_it() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let host = SharedHost::new();
    let (mut running, settings) = start(&fixture, &host);
    let mut client = Client::connect(settings.socket()).unwrap();
    let retired = client
        .call_as_operator(
            dudes_proto::rpc::Method::AgentRetire,
            Some(json!({ "agent": story.hire.id, "reason": "completed" })),
            credential(&settings),
        )
        .unwrap();
    assert!(retired.error.is_none());
    let listed = client
        .call_as_operator(
            dudes_proto::rpc::Method::WorktreeList,
            Some(json!({})),
            credential(&settings),
        )
        .unwrap();
    let copies: Vec<dudes_proto::rpc::worktree::WorktreeEntry> =
        serde_json::from_value(listed.result.unwrap()).unwrap();
    assert_eq!(copies.len(), 3);
    assert!(copies.iter().all(|copy| copy.agent == Some(story.hire.id)
        && copy.state == dudes_proto::WorktreeViewState::Released));
    running.stop(ExitCause::Ended);
    assert!(story.copies().all(|repo| repo.execution.directory.is_dir()));
    assert!(host.spawned().is_empty());
}

#[test]
fn crash_before_the_atomic_hire_discards_every_pending_copy() {
    let fixture = Fixture::new();
    let root = std::fs::canonicalize(fixture.runtime.path()).unwrap();
    let state = std::fs::canonicalize(fixture.state.path()).unwrap();
    let entries = ["api", "docs", "web"]
        .into_iter()
        .map(|name| create_origin(&root, name))
        .collect();
    let catalog = RepositorySet::new(RepoId::parse("api").unwrap(), entries).unwrap();
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), fixture.settings().journal).unwrap();
    let catalog_lsn = append(
        &mut writer,
        Record::ProjectRepositoriesRecorded {
            project: "service".into(),
            repositories: catalog.clone(),
        },
    );
    let Record::WorkspaceTrustSet { pin, .. } = samples::record(RecordKind::WorkspaceTrustSet)
    else {
        unreachable!()
    };
    let mut grants = std::collections::BTreeMap::new();
    for entry in catalog.entries() {
        let trust_lsn = append(
            &mut writer,
            Record::RepositoryTrustSet {
                project: "service".into(),
                repo_id: entry.repo_id.clone(),
                catalog_lsn,
                origin: entry.value.origin.clone(),
                pin: pin.clone(),
            },
        );
        grants.insert(entry.repo_id.clone(), trust_lsn);
    }
    let mut copies = Vec::new();
    for (index, entry) in catalog.entries().iter().take(2).enumerate() {
        copies.push(create_copy(
            &mut writer,
            entry,
            &state,
            index,
            grants[&entry.repo_id],
            LeaseCarrier::WithIdentity,
        ));
    }
    drop(writer);
    assert_eq!(
        copies.len(),
        2,
        "the crash prefix stops before the third lease"
    );
    let host = SharedHost::new();
    let (mut running, _) = start(&fixture, &host);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let removed = fixture
            .records()
            .iter()
            .filter(|record| {
                matches!(
                    record,
                    Record::WorktreeRemoved {
                        how: WorktreeRemoval::NeverAssigned,
                        ..
                    }
                )
            })
            .count();
        if removed == copies.len() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "every pending lease must settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    running.stop(ExitCause::Ended);
    assert!(copies.iter().all(|repo| !repo.execution.directory.exists()));
    assert!(host.spawned().is_empty());
}

#[test]
fn losing_a_nonprimary_copy_holds_the_whole_hire_and_retains_the_other_copies() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let missing = &story.hire.repositories.repositories.entries()[2].value;
    std::fs::rename(
        &missing.execution.directory,
        missing
            .execution
            .directory
            .with_extension("removed-outside-daemon"),
    )
    .unwrap();
    let host = SharedHost::new();
    let (mut running, _) = start(&fixture, &host);
    running.stop(ExitCause::Ended);
    assert!(fixture.records().iter().any(|record| matches!(record, Record::AgentHeld { agent, cause: dudes_proto::AutomaticHold::WorktreeMissing { .. }, .. } if *agent == story.hire.id)));
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::WorktreeRemoved { .. })
                || record.turn_issue().is_some())
    );
    assert!(
        story
            .copies()
            .filter(|repo| repo.worktree != missing.worktree)
            .all(|repo| repo.execution.directory.exists())
    );
    assert!(host.spawned().is_empty());
}

#[test]
fn a_replayed_scoped_goal_is_held_before_any_legacy_issue_or_provider_spawn() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let mut goal = samples::record(RecordKind::GoalSet);
    let Record::GoalSet { agent, .. } = &mut goal else {
        unreachable!()
    };
    *agent = story.hire.id;
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), fixture.settings().journal).unwrap();
    append(&mut writer, goal);
    drop(writer);
    let host = SharedHost::new();
    let (mut running, _) = start(&fixture, &host);
    let before = fixture.entries();
    assert!(
        running
            .handle
            .ask(duded::ports::Ask::Resume(story.hire.id))
            .is_err()
    );
    assert_eq!(fixture.entries(), before);
    running.stop(ExitCause::Ended);
    assert!(
        fixture
            .records()
            .iter()
            .all(|record| !matches!(record, Record::Pause {
        agent, ..
    } if *agent == story.hire.id))
    );
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| record.turn_issue().is_some())
    );
    assert!(host.spawned().is_empty());
}

#[test]
fn a_replaced_nonprimary_copy_is_held_even_when_its_gitfile_still_names_the_origin() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let replaced = &story.hire.repositories.repositories.entries()[2].value;
    let gitfile = std::fs::read(replaced.execution.directory.join(".git")).unwrap();
    std::fs::rename(
        &replaced.execution.directory,
        replaced
            .execution
            .directory
            .with_extension("replaced-outside-daemon"),
    )
    .unwrap();
    std::fs::create_dir(&replaced.execution.directory).unwrap();
    std::fs::write(replaced.execution.directory.join(".git"), gitfile).unwrap();
    assert_ne!(
        identity(&replaced.execution.directory),
        replaced.execution.directory_identity
    );
    let host = SharedHost::new();
    let (mut running, _) = start(&fixture, &host);
    running.stop(ExitCause::Ended);
    assert!(
        fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::AgentHeld {
        agent, cause: dudes_proto::AutomaticHold::WorktreeMissing { worktree }, ..
    } if *agent == story.hire.id && *worktree == replaced.worktree))
    );
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::WorktreeRemoved { .. })
                || record.turn_issue().is_some())
    );
    assert!(story.copies().all(|repo| repo.execution.directory.exists()));
    assert!(host.spawned().is_empty());
}

#[test]
fn malformed_repository_history_refuses_boot_even_when_the_cache_has_advanced_past_it() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let settings = settings(&fixture);
    let reader = Reader::open(&fixture.journal_dir()).unwrap().0;
    let mut cache =
        Projections::open(&settings.state_dir.join(duded::boot::PROJECTION_FILE)).unwrap();
    replay(&reader, &mut cache).unwrap();
    let host = SharedHost::new();
    let seams = common::seams(
        &settings,
        host.clone(),
        Arc::new(RealRunner(OneShot::new())),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    let mut origin = story.copies().next().unwrap().origin.clone();
    origin.directory_identity.inode += 1_000_000;
    let (mut writer, _) = dudes_journal::open(&fixture.journal_dir(), settings.journal).unwrap();
    let bad_lsn = append(
        &mut writer,
        Record::RepositoryTrustSet {
            project: story.hire.repositories.project.id.clone(),
            repo_id: story.hire.repositories.repositories.entries()[0]
                .repo_id
                .clone(),
            catalog_lsn: story.hire.repositories.catalog_lsn,
            origin,
            pin: None,
        },
    );
    let tail = Entry {
        at: Timestamp::from_unix_millis(1_001),
        record: Record::Pause {
            agent: story.hire.id,
            browser: None,
        },
    };
    let (lsn, crc) = writer.append_witnessed(&tail).unwrap();
    drop(writer);
    cache.apply(lsn, crc, &tail).unwrap();
    assert!(cache.last_applied_lsn() > bad_lsn);
    drop(cache);
    let path = dudes_journal::segment::segment_path(&fixture.journal_dir(), bad_lsn.segment());
    let before = std::fs::read(&path).unwrap();
    let result = boot(&settings, seams);
    let error = match result {
        Ok(_) => panic!("malformed repository history was admitted"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains("repository trust grant does not match"),
        "{error}"
    );
    assert!(!settings.socket().exists());
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert!(host.spawned().is_empty());
}

fn replay_pair(fixture: &Fixture) -> (Writer, dudes_supervisor::World, Projections) {
    let (writer, _) =
        dudes_journal::open(&fixture.journal_dir(), fixture.settings().journal).unwrap();
    let reader = Reader::open(&fixture.journal_dir()).unwrap().0;
    let mut projection = Projections::open(&fixture.state.path().join("paired.redb")).unwrap();
    replay(&reader, &mut projection).unwrap();
    let mut world = dudes_supervisor::World::default();
    for item in reader.iter_from(Lsn::ZERO).unwrap() {
        let (lsn, entry) = item.unwrap();
        world.apply_checked(lsn, &entry).unwrap();
    }
    (writer, world, projection)
}

#[test]
fn equivalent_retrust_during_pending_leases_still_refuses_the_atomic_hire() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let source = Reader::open(&fixture.journal_dir()).unwrap().0;
    let pending = fixture.runtime.path().join("pending-journal");
    let (mut writer, _) = dudes_journal::open(&pending, fixture.settings().journal).unwrap();
    let mut world = dudes_supervisor::World::default();
    let mut projection = Projections::in_memory().unwrap();
    let mut first_lease = true;
    for item in source.iter_from(Lsn::ZERO).unwrap() {
        let (_, entry) = item.unwrap();
        if entry.record.hire().is_some() {
            let (lsn, crc) = writer.append_witnessed(&entry).unwrap();
            let before = projection.export().unwrap();
            assert!(matches!(
                projection.apply(lsn, crc, &entry),
                Err(dudes_journal::Error::Repository {
                    reason: dudes_proto::repository_state::history::RepositoryStateError::Trust,
                    ..
                })
            ));
            assert!(world.apply_checked(lsn, &entry).is_err());
            assert_eq!(projection.export().unwrap(), before);
            break;
        }
        let (lsn, crc) = writer.append_witnessed(&entry).unwrap();
        apply_pair(&mut world, &mut projection, &(lsn, crc, entry.clone()));
        if first_lease && matches!(entry.record, Record::WorktreeLeased { .. }) {
            first_lease = false;
            let grant = projection.repository_trust().unwrap().remove(0);
            for pin in [None, grant.pin.clone()] {
                let recorded = write_record(
                    &mut writer,
                    Record::RepositoryTrustSet {
                        project: grant.project.clone(),
                        repo_id: grant.repo_id.clone(),
                        catalog_lsn: grant.catalog_lsn,
                        origin: grant.origin.clone(),
                        pin,
                    },
                );
                apply_pair(&mut world, &mut projection, &recorded);
            }
        }
    }
    assert!(world.agents.is_empty());
    assert!(projection.agent(story.hire.id).unwrap().is_none());
    assert!(
        projection
            .worktrees()
            .unwrap()
            .iter()
            .all(|row| row.agent.is_none())
    );
    assert!(matches!(
        Reader::open(&pending),
        Err(dudes_journal::Error::Repository {
            reason: dudes_proto::repository_state::history::RepositoryStateError::Trust,
            ..
        })
    ));
}

fn write_record(writer: &mut Writer, record: Record) -> (Lsn, u32, Entry) {
    let entry = Entry {
        at: Timestamp::UNIX_EPOCH,
        record,
    };
    let (lsn, crc) = writer.append_witnessed(&entry).unwrap();
    (lsn, crc, entry)
}

fn apply_pair(
    world: &mut dudes_supervisor::World,
    projection: &mut Projections,
    recorded: &(Lsn, u32, Entry),
) {
    projection
        .apply(recorded.0, recorded.1, &recorded.2)
        .unwrap();
    world.apply_checked(recorded.0, &recorded.2).unwrap();
}

fn legacy_hire(agent: AgentId, cwd: &Path) -> Record {
    let mut record = samples::record(RecordKind::AgentHired);
    let Record::AgentHired {
        id,
        cwd: recorded,
        workspace,
        worktree,
        browser_onboarding,
        mcp_servers,
        ..
    } = &mut record
    else {
        unreachable!()
    };
    *id = agent;
    *recorded = cwd.to_path_buf();
    *workspace = None;
    *worktree = None;
    *browser_onboarding = None;
    mcp_servers.clear();
    record
}

fn legacy_issue(agent: AgentId) -> Record {
    let mut record = samples::record(RecordKind::TurnIssued);
    let Record::TurnIssued {
        agent: owner,
        answers,
        ..
    } = &mut record
    else {
        unreachable!()
    };
    *owner = agent;
    answers.clear();
    record
}

fn reject_pair(
    fixture: &Fixture,
    world: &mut dudes_supervisor::World,
    projection: &mut Projections,
    recorded: &(Lsn, u32, Entry),
    expected: dudes_proto::repository_state::history::RepositoryStateError,
) {
    let before = projection.export().unwrap();
    let world_cursor = world.last_applied_lsn;
    let projected = projection.apply(recorded.0, recorded.1, &recorded.2);
    let folded = world.apply_checked(recorded.0, &recorded.2);
    let read = Reader::open(&fixture.journal_dir()).map(|(_, report)| report.frames);
    assert!(
        matches!(&projected, Err(dudes_journal::Error::Repository { reason, .. }) if *reason == expected),
        "projection={projected:?}; world={folded:?}; reader={read:?}"
    );
    assert!(folded.is_err(), "{folded:?}");
    assert!(
        matches!(read, Err(dudes_journal::Error::Repository { reason, .. }) if reason == expected)
    );
    assert_eq!(projection.export().unwrap(), before);
    assert_eq!(world.last_applied_lsn, world_cursor);
}

#[test]
fn retirement_cannot_forget_a_scoped_hires_legacy_issue_refusal() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let (mut writer, mut world, mut projection) = replay_pair(&fixture);
    let retired = write_record(
        &mut writer,
        Record::AgentRetired {
            id: story.hire.id,
            reason: "completed".into(),
            browser_retire: None,
        },
    );
    apply_pair(&mut world, &mut projection, &retired);
    let issue = write_record(&mut writer, legacy_issue(story.hire.id));
    reject_pair(
        &fixture,
        &mut world,
        &mut projection,
        &issue,
        dudes_proto::repository_state::history::RepositoryStateError::LegacyIssue,
    );
}

fn fresh_hire(fixture: &Fixture, writer: &mut Writer, story: &Story) -> RepositoryHire {
    let mut hire = story.hire.clone();
    let state = std::fs::canonicalize(fixture.state.path()).unwrap();
    let mut ordered: Vec<_> = hire.repositories.repositories.entries().iter().collect();
    ordered.sort_by_key(|entry| &entry.repo_id);
    let mut copies = std::collections::BTreeMap::new();
    for (index, entry) in ordered.into_iter().enumerate() {
        let catalog = RepositoryEntry {
            repo_id: entry.repo_id.clone(),
            value: CatalogRepository {
                label: "Copy".into(),
                origin: entry.value.origin.clone(),
            },
        };
        copies.insert(
            entry.repo_id.clone(),
            create_copy(
                writer,
                &catalog,
                &state,
                index + 10,
                entry.value.trust_lsn,
                LeaseCarrier::Legacy,
            ),
        );
    }
    hire.repositories.repositories = RepositorySet::new(
        hire.repositories.repositories.primary_id().clone(),
        hire.repositories
            .repositories
            .entries()
            .iter()
            .map(|entry| RepositoryEntry {
                repo_id: entry.repo_id.clone(),
                value: copies[&entry.repo_id].clone(),
            })
            .collect(),
    )
    .unwrap();
    hire
}

#[test]
fn retirement_cannot_allow_a_second_scoped_hire_with_the_same_agent_id() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let (mut writer, mut world, mut projection) = replay_pair(&fixture);
    let retired = write_record(
        &mut writer,
        Record::AgentRetired {
            id: story.hire.id,
            reason: "completed".into(),
            browser_retire: None,
        },
    );
    apply_pair(&mut world, &mut projection, &retired);
    let hire = fresh_hire(&fixture, &mut writer, &story);
    let reader = Reader::open(&fixture.journal_dir()).unwrap().0;
    replay(&reader, &mut projection).unwrap();
    for item in reader.iter_from(retired.0).unwrap().skip(1) {
        let (lsn, entry) = item.unwrap();
        world.apply_checked(lsn, &entry).unwrap();
    }
    let hired = write_record(
        &mut writer,
        Record::AgentHiredWithRepositories(Box::new(hire)),
    );
    reject_pair(
        &fixture,
        &mut world,
        &mut projection,
        &hired,
        dudes_proto::repository_state::history::RepositoryStateError::Hire,
    );
}

#[test]
fn a_legacy_rehire_cannot_downgrade_scoped_authority_before_an_issue() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let (mut writer, mut world, mut projection) = replay_pair(&fixture);
    let hired = write_record(
        &mut writer,
        legacy_hire(
            story.hire.id,
            &story
                .hire
                .repositories
                .repositories
                .primary()
                .origin
                .directory,
        ),
    );
    write_record(&mut writer, legacy_issue(story.hire.id));
    reject_pair(
        &fixture,
        &mut world,
        &mut projection,
        &hired,
        dudes_proto::repository_state::history::RepositoryStateError::Hire,
    );
}

#[test]
fn legacy_only_retirement_rehire_and_issue_history_remains_readable() {
    let fixture = Fixture::new();
    let (mut writer, mut world, mut projection) = replay_pair(&fixture);
    let agent = AgentId::from_parts(3, 1);
    for record in [
        legacy_hire(agent, fixture.runtime.path()),
        Record::AgentRetired {
            id: agent,
            reason: "completed".into(),
            browser_retire: None,
        },
        legacy_issue(agent),
        legacy_hire(agent, fixture.runtime.path()),
    ] {
        let recorded = write_record(&mut writer, record);
        apply_pair(&mut world, &mut projection, &recorded);
    }
    let reader = Reader::open(&fixture.journal_dir()).unwrap().0;
    let mut rebuilt = Projections::in_memory().unwrap();
    replay(&reader, &mut rebuilt).unwrap();
    assert_eq!(rebuilt.export().unwrap(), projection.export().unwrap());
    assert!(world.agents[&agent].repositories.singleton().is_some());
}

#[test]
fn retired_repository_class_survives_warm_reopen_and_empty_rebuild() {
    use dudes_proto::repository_state::history::RepositoryHireClass;
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let (mut writer, mut world, mut projection) = replay_pair(&fixture);
    let retired = write_record(
        &mut writer,
        Record::AgentRetired {
            id: story.hire.id,
            reason: "completed".into(),
            browser_retire: None,
        },
    );
    apply_pair(&mut world, &mut projection, &retired);
    let expected = projection.export().unwrap();
    drop(projection);
    let reader = Reader::open(&fixture.journal_dir()).unwrap().0;
    let mut warm = Projections::open(&fixture.state.path().join("paired.redb")).unwrap();
    assert!(warm.agent(story.hire.id).unwrap().is_none());
    assert_eq!(
        warm.repository_hire_class(story.hire.id).unwrap(),
        Some(RepositoryHireClass::Scoped)
    );
    assert_eq!(
        world.repository_hires[&story.hire.id],
        RepositoryHireClass::Scoped
    );
    assert_eq!(replay(&reader, &mut warm).unwrap().applied, 0);
    assert_eq!(warm.export().unwrap(), expected);
    let mut rebuilt = Projections::in_memory().unwrap();
    replay(&reader, &mut rebuilt).unwrap();
    assert_eq!(rebuilt.export().unwrap(), expected);
}

fn ticket_start(story: &Story) -> dudes_proto::repository::RepositoryTicketStarted {
    use dudes_proto::repository::{TicketDeclaration, TicketStartRepositories};
    use dudes_proto::{CheckBaseline, Goal, GoalBudget, Predicate, TicketCheck};
    use dudes_types::{CardId, GoalId, Seq};
    let copies = story.hire.repositories.repositories.entries();
    let check = TicketCheck {
        id: "verify".into(),
        label: "verification passes".into(),
        predicate: Predicate::Command {
            argv: vec!["/usr/bin/true".into()],
            timeout_s: 30,
        },
    };
    dudes_proto::repository::RepositoryTicketStarted {
        card: CardId::from_parts(7, 1),
        agent: story.hire.id,
        expected_seq: Seq(2),
        round: 1,
        goal: Goal {
            id: GoalId::from_parts(8, 1),
            text: "Repair the web and API".into(),
            predicate: check.predicate.clone(),
            completion_check: duded::scoped_ticket_goal_check(
                &check,
                "service",
                &copies[1].repo_id,
            ),
            baseline: Some(CheckBaseline::MayAlreadyPass),
            budget: GoalBudget {
                usd: None,
                tokens: None,
            },
            hop_cap: 4,
        },
        repositories: TicketStartRepositories {
            hired_lsn: story.hired_lsn,
            declaration: TicketDeclaration {
                project: "service".into(),
                catalog_lsn: story.hire.repositories.catalog_lsn,
                repositories: RepositorySet::new(
                    copies[0].repo_id.clone(),
                    copies
                        .iter()
                        .take(2)
                        .map(|copy| RepositoryEntry {
                            repo_id: copy.repo_id.clone(),
                            value: copy.value.origin.clone(),
                        })
                        .collect(),
                )
                .unwrap(),
            },
            check,
            check_repo: copies[1].repo_id.clone(),
            check_root: copies[1].value.execution.clone(),
        },
    }
}

fn seed_ticket(writer: &mut Writer, start: &dudes_proto::repository::RepositoryTicketStarted) {
    use dudes_proto::Actor;
    use dudes_proto::repository::{RepositoryTicketCreated, RepositoryTicketEdited};
    use dudes_types::Seq;
    append(
        writer,
        Record::TicketCreatedWithRepositories(Box::new(RepositoryTicketCreated {
            card: start.card,
            title: start.goal.text.clone(),
            description: "Keep both changes together.".into(),
            department: "eng".into(),
            declaration: start.repositories.declaration.clone(),
            done_when: start.repositories.check.clone(),
            check_repo: start.repositories.check_repo.clone(),
            by: Actor::System,
            request: None,
        })),
    );
    append(
        writer,
        Record::TicketEditedWithRepositories(Box::new(RepositoryTicketEdited {
            card: start.card,
            title: start.goal.text.clone(),
            description: "Review both repositories.".into(),
            done_when: start.repositories.check.clone(),
            check_repo: start.repositories.check_repo.clone(),
            by: Actor::System,
            expected_seq: Seq::ZERO,
        })),
    );
    append(
        writer,
        Record::TicketAssigned {
            card: start.card,
            agent: start.agent,
            by: Actor::System,
            message: None,
            expected_seq: Seq(1),
            priority: None,
        },
    );
}

#[test]
fn a_declared_ticket_boots_and_reads_with_frozen_authority_without_enabling_execution() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let start = ticket_start(&story);
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), fixture.settings().journal).unwrap();
    seed_ticket(&mut writer, &start);
    let start_lsn = append(
        &mut writer,
        Record::TicketStartedWithRepositories(Box::new(start.clone())),
    );
    drop(writer);
    let host = SharedHost::new();
    let mut settings = fixture.settings();
    settings.floor_board_enabled = true;
    let daemon = boot(
        &settings,
        common::seams(
            &settings,
            host.clone(),
            Arc::new(RealRunner(OneShot::new())),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let world_round = daemon
        .world()
        .ticket_repository_round(start.goal.id)
        .unwrap();
    assert_eq!(world_round.start_lsn, start_lsn);
    assert_eq!(world_round.round, 1);
    assert_eq!(world_round.authority, Some(start.repositories.clone()));
    assert_eq!(
        daemon.world().agents[&start.agent].goal,
        Some(start.goal.id)
    );
    let mut running = common::start(daemon);
    let response = Client::connect(settings.socket())
        .unwrap()
        .call_as_operator(
            dudes_proto::rpc::Method::TicketRead,
            Some(json!({"card": start.card})),
            credential(&settings),
        )
        .unwrap();
    assert!(response.error.is_none(), "{:?}", response.error);
    let body = response.result.unwrap();
    assert_eq!(body["ticket"]["project"], "service");
    assert!(body["ticket"]["project"].is_string());
    let read: dudes_proto::rpc::ticket::TicketReadResult = serde_json::from_value(body).unwrap();
    let scope = read.ticket.repositories.declared().unwrap();
    assert_eq!(scope.declaration, start.repositories.declaration);
    assert_eq!(scope.check_repo, RepoId::parse("api").unwrap());
    assert_eq!(
        scope.declaration.repositories.primary_id(),
        &RepoId::parse("web").unwrap()
    );
    assert_eq!(
        start.repositories.check_root,
        story
            .hire
            .repositories
            .repositories
            .get(&scope.check_repo)
            .unwrap()
            .execution
    );
    running.stop(ExitCause::Ended);
    assert!(host.spawned().is_empty());
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| record.turn_issue().is_some())
    );
    let reader = Reader::open(&fixture.journal_dir()).unwrap().0;
    let mut warm =
        Projections::open(&settings.state_dir.join(duded::boot::PROJECTION_FILE)).unwrap();
    assert_eq!(replay(&reader, &mut warm).unwrap().applied, 0);
    let expected = warm.export().unwrap();
    assert_eq!(
        warm.goal(start.goal.id)
            .unwrap()
            .unwrap()
            .repository_round
            .as_ref()
            .unwrap()
            .authority,
        Some(start.repositories)
    );
    let mut fresh = Projections::in_memory().unwrap();
    replay(&reader, &mut fresh).unwrap();
    assert_eq!(fresh.export().unwrap(), expected);
}

#[test]
fn world_and_cache_refuse_a_reused_declared_start_without_partial_goal_installation() {
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let start = ticket_start(&story);
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), fixture.settings().journal).unwrap();
    seed_ticket(&mut writer, &start);
    drop(writer);
    let (mut writer, mut world, mut projection) = replay_pair(&fixture);
    let valid = write_record(
        &mut writer,
        Record::TicketStartedWithRepositories(Box::new(start.clone())),
    );
    apply_pair(&mut world, &mut projection, &valid);
    let duplicate = write_record(
        &mut writer,
        Record::TicketStartedWithRepositories(Box::new(start.clone())),
    );
    reject_pair(
        &fixture,
        &mut world,
        &mut projection,
        &duplicate,
        dudes_proto::repository_state::history::RepositoryStateError::Start,
    );
    assert_eq!(
        world
            .ticket_repository_round(start.goal.id)
            .unwrap()
            .start_lsn,
        valid.0
    );
    assert_eq!(projection.ticket(start.card).unwrap().unwrap().1.round, 1);
}

#[test]
fn legacy_ticket_producers_refuse_declared_work_without_appending_any_record() {
    use dudes_proto::rpc::{ErrorCode, Method};
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let mut ticket = ticket_start(&story);
    let legacy = AgentId::from_parts(3, 99);
    ticket.agent = legacy;
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), fixture.settings().journal).unwrap();
    let mut hired = legacy_hire(legacy, fixture.runtime.path());
    let Record::AgentHired { workspace, .. } = &mut hired else {
        unreachable!()
    };
    *workspace = Some(RecordedWorkspace {
        id: "service".into(),
        label: "Service".into(),
    });
    append(&mut writer, hired);
    seed_ticket(&mut writer, &ticket);
    drop(writer);
    let host = SharedHost::new();
    let mut settings = fixture.settings();
    settings.floor_board_enabled = true;
    let daemon = boot(
        &settings,
        common::seams(
            &settings,
            host.clone(),
            Arc::new(RealRunner(OneShot::new())),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let mut client = Client::connect(settings.socket()).unwrap();
    let read = client
        .call_as_operator(
            Method::TicketRead,
            Some(json!({"card": ticket.card})),
            credential(&settings),
        )
        .unwrap();
    assert!(read.error.is_none());
    let read: dudes_proto::rpc::ticket::TicketReadResult =
        serde_json::from_value(read.result.unwrap()).unwrap();
    assert_eq!(read.card.status, dudes_types::CardStatus::Blocked);
    assert_eq!(
        read.ticket.reason,
        Some(dudes_proto::TicketReason::ProjectUnavailable)
    );
    assert_eq!(read.card.claimed_by, Some(legacy));
    let before_lsn = Reader::open(&fixture.journal_dir()).unwrap().1.end;
    let path = dudes_journal::segment::segment_path(&fixture.journal_dir(), before_lsn.segment());
    let before_bytes = std::fs::read(&path).unwrap();
    for (method, params) in [
        (
            Method::TicketAssign,
            json!({"card": ticket.card, "agent": legacy, "expected_seq": read.card.seq}),
        ),
        (
            Method::TicketEdit,
            json!({"card": ticket.card, "title": "Other title", "expected_seq": read.card.seq}),
        ),
    ] {
        let response = client
            .call_as_operator(method, Some(params), credential(&settings))
            .unwrap();
        let error = response.error.unwrap();
        assert_eq!(error.code, ErrorCode::Refused.code());
        let reason = "select a project with an explicit repository list";
        assert!(error.message.ends_with(reason), "{method:?}: {error:?}");
        assert_eq!(
            Reader::open(&fixture.journal_dir()).unwrap().1.end,
            before_lsn
        );
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
    }
    let response = client
        .call_current_as_operator(
            dudes_proto::rpc::CurrentMethod::TicketCheckout,
            Some(json!({"card": ticket.card, "expected_seq": read.card.seq})),
            credential(&settings),
        )
        .unwrap();
    assert_eq!(response.error.unwrap().code, ErrorCode::Refused.code());
    assert_eq!(
        Reader::open(&fixture.journal_dir()).unwrap().1.end,
        before_lsn
    );
    assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| record.ticket_start().is_some() || record.turn_issue().is_some())
    );
    running.stop(ExitCause::Ended);
    assert!(host.spawned().is_empty());
}

#[test]
fn stale_legacy_assignment_claim_and_transition_keep_the_same_scoped_start_facts() {
    use dudes_proto::Actor;
    use dudes_types::{CardStatus, Seq};
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let mut start = ticket_start(&story);
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), fixture.settings().journal).unwrap();
    seed_ticket(&mut writer, &start);
    drop(writer);
    let (mut writer, mut world, mut projection) = replay_pair(&fixture);
    for record in [
        Record::TicketAssigned {
            card: start.card,
            agent: AgentId::NIL,
            by: Actor::System,
            message: None,
            expected_seq: Seq::ZERO,
            priority: None,
        },
        Record::Claim {
            card: start.card,
            agent: start.agent,
            expected_seq: Seq::ZERO,
        },
        Record::CardTransition {
            card: start.card,
            from: CardStatus::Doing,
            to: CardStatus::Claimed,
            by: Actor::System,
            expected_seq: Seq::ZERO,
            reason: None,
        },
    ] {
        let recorded = write_record(&mut writer, record);
        apply_pair(&mut world, &mut projection, &recorded);
    }
    let (card, ticket) = projection.ticket(start.card).unwrap().unwrap();
    assert_eq!(card.seq, Seq(1));
    assert_eq!(card.claimed_by, Some(start.agent));
    assert!(ticket.queue.is_some());
    start.expected_seq = Seq(1);
    let recorded = write_record(
        &mut writer,
        Record::TicketStartedWithRepositories(Box::new(start.clone())),
    );
    apply_pair(&mut world, &mut projection, &recorded);
    assert_eq!(
        projection
            .goal(start.goal.id)
            .unwrap()
            .unwrap()
            .repository_round
            .as_ref(),
        world.ticket_repository_round(start.goal.id)
    );
    let mut rebuilt = Projections::in_memory().unwrap();
    replay(
        &Reader::open(&fixture.journal_dir()).unwrap().0,
        &mut rebuilt,
    )
    .unwrap();
    assert_eq!(rebuilt.export().unwrap(), projection.export().unwrap());
}

#[test]
fn removing_a_plain_card_cannot_free_its_id_for_a_declared_ticket() {
    use dudes_proto::Actor;
    use dudes_types::Seq;
    let fixture = Fixture::new();
    let story = Story::seed(&fixture);
    let start = ticket_start(&story);
    let (mut writer, _) =
        dudes_journal::open(&fixture.journal_dir(), fixture.settings().journal).unwrap();
    seed_ticket(&mut writer, &start);
    drop(writer);
    let created = fixture
        .records()
        .into_iter()
        .find(|record| record.ticket_created().is_some())
        .unwrap();
    let (mut writer, mut world, mut projection) = replay_pair(&fixture);
    let removal = Record::TicketRemoved {
        card: start.card,
        by: Actor::System,
        expected_seq: Seq::ZERO,
    };
    for record in [
        removal.clone(),
        created.clone(),
        Record::CardCreated {
            card: start.card,
            title: "A plain card now owns this identity".into(),
            department: "eng".into(),
            by: Actor::System,
            deps: Vec::new(),
        },
        removal,
    ] {
        let recorded = write_record(&mut writer, record);
        apply_pair(&mut world, &mut projection, &recorded);
    }
    assert!(projection.card(start.card).unwrap().is_some());
    assert!(projection.ticket(start.card).unwrap().is_none());
    let recorded = write_record(&mut writer, created);
    reject_pair(
        &fixture,
        &mut world,
        &mut projection,
        &recorded,
        dudes_proto::repository_state::history::RepositoryStateError::Ticket,
    );
}

#[test]
fn retirement_preserves_review_and_releases_queued_ticket_facts_in_both_folds() {
    use dudes_proto::Actor;
    use dudes_proto::repository::RepositoryTicketEdited;
    use dudes_types::{CardStatus, Seq};
    for review in [false, true] {
        let fixture = Fixture::new();
        let story = Story::seed(&fixture);
        let start = ticket_start(&story);
        let (mut writer, _) =
            dudes_journal::open(&fixture.journal_dir(), fixture.settings().journal).unwrap();
        seed_ticket(&mut writer, &start);
        drop(writer);
        let (mut writer, mut world, mut projection) = replay_pair(&fixture);
        if review {
            let recorded = write_record(
                &mut writer,
                Record::CardTransition {
                    card: start.card,
                    from: CardStatus::Claimed,
                    to: CardStatus::Review,
                    by: Actor::System,
                    expected_seq: Seq(2),
                    reason: None,
                },
            );
            apply_pair(&mut world, &mut projection, &recorded);
        }
        let retired = write_record(
            &mut writer,
            Record::AgentRetired {
                id: start.agent,
                reason: "completed".into(),
                browser_retire: None,
            },
        );
        apply_pair(&mut world, &mut projection, &retired);
        let (card, ticket) = projection.ticket(start.card).unwrap().unwrap();
        assert_eq!(card.seq, Seq(3));
        assert_eq!(
            card.status,
            if review {
                CardStatus::Review
            } else {
                CardStatus::Todo
            }
        );
        assert_eq!(card.claimed_by, review.then_some(start.agent));
        assert!(ticket.queue.is_none());
        let edited = write_record(
            &mut writer,
            Record::TicketEditedWithRepositories(Box::new(RepositoryTicketEdited {
                card: start.card,
                title: start.goal.text,
                description: String::new(),
                done_when: start.repositories.check,
                check_repo: start.repositories.check_repo,
                by: Actor::System,
                expected_seq: Seq(3),
            })),
        );
        if review {
            reject_pair(
                &fixture,
                &mut world,
                &mut projection,
                &edited,
                dudes_proto::repository_state::history::RepositoryStateError::Ticket,
            );
        } else {
            apply_pair(&mut world, &mut projection, &edited);
            let mut rebuilt = Projections::in_memory().unwrap();
            replay(
                &Reader::open(&fixture.journal_dir()).unwrap().0,
                &mut rebuilt,
            )
            .unwrap();
            assert_eq!(rebuilt.export().unwrap(), projection.export().unwrap());
        }
    }
}

#[test]
fn historical_orphan_starts_keep_the_same_rounds_without_a_declaration() {
    let fixture = Fixture::new();
    let (mut writer, mut world, mut projection) = replay_pair(&fixture);
    let hired = write_record(&mut writer, samples::record(RecordKind::AgentHired));
    apply_pair(&mut world, &mut projection, &hired);
    let mut serial = 0_u128;
    for replacement in [
        None,
        Some(RecordKind::CardCreated),
        Some(RecordKind::TicketCreated),
    ] {
        if let Some(kind) = replacement {
            for record in [
                samples::record(kind),
                samples::record(RecordKind::TicketRemoved),
            ] {
                let recorded = write_record(&mut writer, record);
                apply_pair(&mut world, &mut projection, &recorded);
            }
        }
        for index in 1_u32..=2 {
            serial += 1;
            let mut record = samples::record(RecordKind::TicketStarted);
            let Record::TicketStarted { goal, .. } = &mut record else {
                unreachable!()
            };
            goal.id = dudes_types::GoalId::from_parts(8, serial);
            let goal = goal.id;
            let recorded = write_record(&mut writer, record);
            apply_pair(&mut world, &mut projection, &recorded);
            let expected = world.ticket_repository_round(goal).unwrap();
            assert_eq!(expected.round, index);
            assert!(expected.repositories.is_none());
            assert!(expected.authority.is_none());
            assert_eq!(
                projection
                    .goal(goal)
                    .unwrap()
                    .unwrap()
                    .repository_round
                    .as_ref(),
                Some(expected)
            );
        }
    }
    let expected = projection.export().unwrap();
    let card = world
        .ticket_repository_round(dudes_types::GoalId::from_parts(8, serial))
        .unwrap()
        .card;
    assert!(expected.contains(&format!("repository_ticket_rounds {card} 2\n")));
    drop(projection);
    let reader = Reader::open(&fixture.journal_dir()).unwrap().0;
    let mut warm = Projections::open(&fixture.state.path().join("paired.redb")).unwrap();
    assert_eq!(replay(&reader, &mut warm).unwrap().applied, 0);
    assert_eq!(warm.export().unwrap(), expected);
    let mut rebuilt = Projections::in_memory().unwrap();
    replay(&reader, &mut rebuilt).unwrap();
    assert_eq!(rebuilt.export().unwrap(), expected);
}
#[path = "repository_replay/scoped_issues.rs"]
mod scoped_issues;

#[path = "repository_replay/scoped_transport.rs"]
mod scoped_transport;

#[path = "repository_replay/review_batches.rs"]
mod review_batches;
