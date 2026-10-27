//! Restricted repository history is readable while provider execution remains unavailable.
#![cfg(unix)]

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use duded::predicate::{PredicateOutcome, PredicateRunner};
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_host::{OneShot, OneShotOutcome};
use dudes_journal::{Projections, Reader, Writer, replay};
use dudes_proto::record::{FileIdentity, RecordedWorkspace, samples};
use dudes_proto::repository::{
    CatalogRepository, ExecutionRepository, HireRepositories, OwnedRepository, RepoId,
    RepositoryEntry, RepositoryHire, RepositoryPolicy, RepositorySet, ScopedRepositories,
};
use dudes_proto::{Entry, Record, RecordKind, WorktreeRemoval};
use dudes_rpc::Client;
use dudes_types::{AgentId, GitCommit, Lsn, Timestamp, WorktreeId};
use serde_json::json;

mod common;
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
}

struct CleanupRun {
    operation: &'static str,
    program: String,
    started: Duration,
    returned: Option<Duration>,
    outcome: Option<String>,
    owned_after_return: Option<usize>,
}

struct CleanupRunner {
    real: RealRunner,
    started: Instant,
    runs: Mutex<Vec<CleanupRun>>,
}

impl CleanupRunner {
    fn new() -> Self {
        Self {
            real: RealRunner(OneShot::new()),
            started: Instant::now(),
            runs: Mutex::new(Vec::new()),
        }
    }

    fn snapshot(&self) -> Vec<String> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .map(|run| {
                format!(
                    "{} program={:?}, started={:?}, returned={:?}, outcome={:?}, owned_after_return={:?}",
                    run.operation, run.program, run.started, run.returned, run.outcome, run.owned_after_return
                )
            })
            .collect()
    }

    fn state_snapshot(&self) -> String {
        let runner = format!("{:?}", self.real.0);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let child_exit = {
            use rustix::process::{WaitId, WaitIdOptions, waitid};
            format!(
                "{:?}",
                waitid(
                    WaitId::All,
                    WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT
                )
            )
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let child_exit = "unavailable";
        let mut snapshot = format!("runner={runner}; unreaped_child_exit={child_exit}");
        let mut end = snapshot.len().min(2048);
        while !snapshot.is_char_boundary(end) {
            end -= 1;
        }
        snapshot.truncate(end);
        snapshot
    }
}

impl PredicateRunner for CleanupRunner {
    fn run(
        &self,
        spec: &dudes_providers::launch::LaunchSpec,
        timeout: Duration,
    ) -> io::Result<PredicateOutcome> {
        let operation = if spec
            .argv
            .windows(2)
            .any(|args| args[0] == "worktree" && args[1] == "remove")
        {
            "worktree remove"
        } else if spec
            .argv
            .windows(2)
            .any(|args| args[0] == "branch" && args[1] == "-D")
        {
            "branch delete"
        } else {
            return self.real.run(spec, timeout);
        };
        let slot = {
            let mut runs = self.runs.lock().unwrap();
            let slot = runs.len();
            if slot < 8 {
                runs.push(CleanupRun {
                    operation,
                    program: spec.argv[0].to_string_lossy().chars().take(256).collect(),
                    started: self.started.elapsed(),
                    returned: None,
                    outcome: None,
                    owned_after_return: None,
                });
                Some(slot)
            } else {
                None
            }
        };
        let result = self.real.run(spec, timeout);
        if let Some(slot) = slot {
            let outcome = match &result {
                Ok(PredicateOutcome::Finished {
                    exit,
                    output_tail,
                    timed_out,
                }) => format!(
                    "exit={exit:?}, timed_out={timed_out}, tail={:?}",
                    output_tail.chars().take(512).collect::<String>()
                ),
                Ok(PredicateOutcome::Swept) => "swept".to_owned(),
                Err(error) => format!(
                    "io_kind={:?}, errno={:?}, message={:?}",
                    error.kind(),
                    error.raw_os_error(),
                    error.to_string().chars().take(512).collect::<String>()
                ),
            };
            let mut runs = self.runs.lock().unwrap();
            runs[slot].returned = Some(self.started.elapsed());
            runs[slot].outcome = Some(outcome);
            runs[slot].owned_after_return = Some(self.real.0.in_flight());
        }
        result
    }

    fn sweep(&self) {
        self.real.sweep();
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let git = duded::worktree::git::Git::new(
        Arc::new(RealRunner(OneShot::new())),
        std::env::var_os("PATH"),
        Duration::from_secs(3),
    );
    let args: Vec<_> = args.iter().map(std::ffi::OsStr::new).collect();
    git.exec(cwd, &args, Duration::from_secs(3)).unwrap()
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

fn create_copy(
    writer: &mut Writer,
    entry: &RepositoryEntry<CatalogRepository>,
    state: &Path,
    index: usize,
    trust_lsn: Lsn,
) -> OwnedRepository {
    let worktree = WorktreeId::from_bytes([u8::try_from(index + 1).unwrap(); 16]);
    let path = state.join("worktrees").join(worktree.to_string());
    let origin = &entry.value.origin;
    let branch = format!("dudes/{}-{worktree}", entry.repo_id.as_str());
    let base = GitCommit::parse(&git(&origin.directory, &["rev-parse", "HEAD"])).unwrap();
    append(
        writer,
        Record::WorktreeLeased {
            worktree,
            origin: origin.directory.clone(),
            path: path.clone(),
            branch: branch.clone(),
            base,
            from: None,
        },
    );
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
                create_copy(&mut writer, entry, &state, index, grants[&entry.repo_id]),
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

fn start(fixture: &Fixture, host: &SharedHost) -> (common::Running, duded::Settings) {
    start_with_runner(fixture, host, Arc::new(RealRunner(OneShot::new())))
}

fn start_with_runner<P: PredicateRunner>(
    fixture: &Fixture,
    host: &SharedHost,
    runner: Arc<P>,
) -> (common::Running, duded::Settings) {
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        common::seams(
            &settings,
            host.clone(),
            runner,
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .unwrap();
    (common::start(daemon), settings)
}

fn await_pending_cleanup(fixture: &Fixture, runner: &CleanupRunner, copies: &[OwnedRepository]) {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(3);
    let mut previous_poll = started;
    let (mut polls, mut max_read, mut max_gap) = (0, Duration::ZERO, Duration::ZERO);
    loop {
        let poll = Instant::now();
        max_gap = max_gap.max(poll.duration_since(previous_poll));
        let entries = fixture.entries();
        previous_poll = Instant::now();
        max_read = max_read.max(previous_poll.duration_since(poll));
        polls += 1;
        let removed = entries
            .iter()
            .filter(|(_, entry)| {
                matches!(
                    entry.record,
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
            Instant::now() < deadline,
            "every pending lease must settle; [DEBUG-cleanup] observer_started={:?}, elapsed={:?}, polls={polls}, max_read={max_read:?}, max_gap={max_gap:?}, \
             last_lsn={:?}, records={:?}, copies_present={:?}, git_admin_present={:?}, git_runs={:?}, runner_state={}",
            started.duration_since(runner.started),
            started.elapsed(),
            entries.last().map(|(lsn, _)| lsn),
            entries
                .iter()
                .rev()
                .take(16)
                .map(|(_, entry)| entry.record.kind())
                .collect::<Vec<_>>(),
            copies
                .iter()
                .map(|copy| copy.execution.directory.exists())
                .collect::<Vec<_>>(),
            copies
                .iter()
                .map(|copy| copy.execution.git_dir.exists())
                .collect::<Vec<_>>(),
            runner.snapshot(),
            runner.state_snapshot(),
        );
        std::thread::sleep(Duration::from_millis(5));
    }
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
    let settings = fixture.settings();
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
    assert_eq!(
        agent.repositories,
        HireRepositories::Scoped(story.hire.repositories.clone())
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
    assert_eq!(
        row.repositories,
        HireRepositories::Scoped(story.hire.repositories.clone())
    );
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
    for (index, entry) in catalog.entries().iter().take(1).enumerate() {
        copies.push(create_copy(
            &mut writer,
            entry,
            &state,
            index,
            grants[&entry.repo_id],
        ));
    }
    drop(writer);
    assert_eq!(
        copies.len(),
        1,
        "the crash prefix stops before the second lease"
    );
    let host = SharedHost::new();
    let runner = Arc::new(CleanupRunner::new());
    let (mut running, _) = start_with_runner(&fixture, &host, Arc::clone(&runner));
    await_pending_cleanup(&fixture, &runner, &copies);
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
    running.stop(ExitCause::Ended);
    assert!(
        fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::AgentHeld {
        agent, cause: dudes_proto::AutomaticHold::LaunchRefused { .. }, ..
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
    let settings = fixture.settings();
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
            create_copy(writer, &catalog, &state, index + 10, entry.value.trust_lsn),
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
