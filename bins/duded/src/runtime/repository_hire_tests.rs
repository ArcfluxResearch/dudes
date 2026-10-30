use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use dudes_config::workspace::{NewWorkspace, RepositoryDefinition};
use dudes_proto::repository::{RepositoryEntry, RepositorySet};
use dudes_proto::rpc::agent::{Checkout, HireParams};
use dudes_proto::{Record, WorktreeRemoval};
use dudes_types::{IsolationMode, Provider};

use super::work_test_support as common;
use super::*;
use crate::worktree::git::Git;
use crate::worktree::hire::{RepositoryHirePlan, WorktreeHires};
pub(super) use common::repository_runner::Runner;

mod browser_groups;

pub(super) struct Setup {
    pub(super) fixture: common::Fixture,
    roots: tempfile::TempDir,
    pub(super) settings: crate::Settings,
    runner: Arc<Runner>,
    git: Git,
    pub(super) host: common::SharedHost,
}

impl Setup {
    pub(super) fn new() -> Self {
        Self::with_repositories(&["web", "api", "docs"])
    }

    pub(super) fn with_repositories(names: &[&str]) -> Self {
        let fixture = common::Fixture::new();
        let roots = tempfile::tempdir().unwrap();
        let root = roots.path().canonicalize().unwrap();
        let runner = Arc::new(Runner::default());
        let git = Git::new(
            runner.clone(),
            std::env::var_os("PATH"),
            Duration::from_secs(3),
        );
        let mut entries = Vec::new();
        for &name in names {
            let path = root.join(name);
            std::fs::create_dir(&path).unwrap();
            command(&git, &path, &["init", "--quiet", "-b", "main"]);
            std::fs::write(path.join("tracked.txt"), "initial\n").unwrap();
            command(&git, &path, &["add", "tracked.txt"]);
            command(
                &git,
                &path,
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
            entries.push(RepositoryEntry {
                repo_id: repo_id(name),
                value: RepositoryDefinition {
                    label: name.to_owned(),
                    path,
                },
            });
        }
        let config = fixture.runtime.path().join("operator.toml");
        let new = NewWorkspace::with_defaults(
            entries[0].value.path.clone(),
            "service".into(),
            "Service".into(),
            dudes_config::check::changed_check(),
        );
        dudes_config::workspace::add(&config, &new).unwrap();
        dudes_config::workspace::set_repositories(
            &config,
            "service",
            &RepositorySet::new(repo_id("api"), entries).unwrap(),
        )
        .unwrap();
        let mut settings = fixture.settings();
        settings.state_dir = settings.state_dir.canonicalize().unwrap();
        settings.predicate_path = std::env::var_os("PATH");
        settings.enrollment = crate::config::load_operator(&config).unwrap().enrollment;
        settings.operator_config = Some(config);
        record_catalog(&fixture, &settings);
        Self {
            fixture,
            roots,
            settings,
            runner,
            git,
            host: common::SharedHost::new(),
        }
    }

    pub(super) fn boot(
        &self,
        capability: bool,
    ) -> Daemon<common::SharedHost, Runner, common::TestClock, common::FixtureRng> {
        let mut seams = common::seams(
            &self.settings,
            self.host.clone(),
            Arc::new(Runner::default()),
            Arc::new(common::TestClock::new()),
            Witness::default(),
        );
        seams.adapters = dudes_providers::registry::Adapters::new([
            Box::new(dudes_providers::claude::ClaudeAdapter::new(
                dudes_providers::claude::config::ClaudeConfig::default(),
            )) as Box<dyn dudes_providers::adapter::Adapter>,
            Box::new(dudes_providers::codex::CodexAdapter::new(
                dudes_providers::codex::config::CodexConfig::default(),
            )),
            Box::new(dudes_providers::pi::PiAdapter::new(
                dudes_providers::pi::config::PiConfig::default(),
            )),
        ]);
        let mut daemon = crate::boot(&self.settings, seams).unwrap();
        daemon.ctx.current_repositories.test_hire_capability = capability;
        daemon
    }

    pub(super) fn start(&self, capability: bool) -> (common::Running, WorktreeHires, HireParams) {
        self.start_from(self.boot(capability))
    }

    fn start_from(
        &self,
        daemon: Daemon<common::SharedHost, Runner, common::TestClock, common::FixtureRng>,
    ) -> (common::Running, WorktreeHires, HireParams) {
        let target = daemon.repository_hire_target("service").unwrap();
        let running = common::start(daemon);
        let hires = WorktreeHires::new(running.handle.clone(), self.git.clone());
        for name in self.settings.enrollment.profiles[0]
            .repositories
            .as_ref()
            .unwrap()
            .entries()
            .iter()
            .map(|entry| entry.repo_id.as_str())
        {
            hires
                .trust(&dudes_proto::rpc::workspace::TrustParams {
                    workspace_id: "service".into(),
                    repo_id: Some(repo_id(name)),
                    trusted: true,
                })
                .unwrap();
        }
        let mut params = common::hire_params(Path::new(""));
        params.project = Some(target);
        params.checkout = Some(Checkout::Worktree);
        params.trusted = true;
        params.model = Some(dudes_types::ModelName::parse("claude-sonnet-4-6").unwrap());
        (running, hires, params)
    }

    fn admitted(
        &self,
    ) -> (
        Daemon<common::SharedHost, Runner, common::TestClock, common::FixtureRng>,
        RepositoryHirePlan,
    ) {
        let (mut running, _, params) = self.start(true);
        running.stop(ExitCause::Ended);
        let daemon = self.boot(true);
        let plan = daemon.repository_hire_plan(params).unwrap();
        (daemon, plan)
    }

    fn assert_cleaned(&self, count: usize) {
        let records = self.fixture.records();
        assert_eq!(
            records
                .iter()
                .filter(|record| record.worktree_lease().is_some())
                .count(),
            count
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record,
                    Record::WorktreeRemoved {
                        how: WorktreeRemoval::NeverAssigned,
                        ..
                    }
                ))
                .count(),
            count
        );
        for record in &records {
            if let Some(lease) = record.worktree_lease() {
                assert!(lease.identity.is_some());
                assert!(!lease.path.exists());
                assert_eq!(
                    self.git
                        .commit(lease.origin, &format!("refs/heads/{}", lease.branch))
                        .unwrap(),
                    lease.base
                );
            }
            assert!(record.hire().is_none());
        }
        assert!(self.host.spawned().is_empty());
    }
}

fn record_catalog(fixture: &common::Fixture, settings: &crate::Settings) {
    let configured = settings
        .enrollment
        .profile("service")
        .unwrap()
        .repositories
        .as_ref()
        .unwrap();
    let catalog = RepositorySet::new(
        configured.primary_id().clone(),
        configured
            .entries()
            .iter()
            .map(|entry| {
                let directory = entry.value.path.clone();
                let common_dir = crate::worktree::plain_git_dir(&directory).unwrap();
                RepositoryEntry {
                    repo_id: entry.repo_id.clone(),
                    value: dudes_proto::repository::CatalogRepository {
                        label: entry.value.label.clone(),
                        origin: dudes_proto::record::TrustedRepository {
                            directory_identity: crate::worktree::trust::identity(&directory)
                                .unwrap(),
                            common_identity: crate::worktree::trust::identity(&common_dir).unwrap(),
                            directory,
                            common_dir,
                        },
                    },
                }
            })
            .collect(),
    )
    .unwrap();
    let (mut writer, _) = dudes_journal::open(&fixture.journal_dir(), settings.journal).unwrap();
    writer
        .append(&Entry {
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_600_000),
            record: Record::ProjectRepositoriesRecorded {
                project: "service".into(),
                repositories: catalog,
            },
        })
        .unwrap();
    drop(writer);
}

fn repo_id(name: &str) -> dudes_proto::repository::RepoId {
    dudes_proto::repository::RepoId::parse(name).unwrap()
}

fn command(git: &Git, cwd: &Path, args: &[&str]) -> String {
    git.exec(
        cwd,
        &args.iter().map(OsStr::new).collect::<Vec<_>>(),
        Duration::from_secs(3),
    )
    .unwrap()
}

#[test]
fn production_project_capabilities_refuse_before_any_lease() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start(false);
    for provider in [Provider::Claude, Provider::Codex, Provider::Pi] {
        let mut request = params.clone();
        request.provider = provider;
        let error = hires.hire(request).unwrap_err();
        assert!(
            error.message.contains("no verified provider capability"),
            "{error:?}"
        );
    }
    running.stop(ExitCause::Ended);
    setup.assert_cleaned(0);
}

#[test]
fn unsupported_project_providers_and_ignored_options_refuse_before_any_lease() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start(true);
    for provider in [Provider::Codex, Provider::Pi] {
        let mut request = params.clone();
        request.provider = provider;
        assert!(
            hires
                .hire(request)
                .unwrap_err()
                .message
                .contains("no verified provider capability")
        );
    }
    for isolation in [
        IsolationMode::Default,
        IsolationMode::Bare,
        IsolationMode::Inherit,
    ] {
        let mut request = params.clone();
        request.isolation = Some(isolation);
        assert!(
            hires
                .hire(request)
                .unwrap_err()
                .message
                .contains("isolation override")
        );
    }
    for model in [None, Some(dudes_types::ModelName::parse("sonnet").unwrap())] {
        let mut request = params.clone();
        request.model = model;
        assert!(
            hires
                .hire(request)
                .unwrap_err()
                .message
                .contains("explicit claude-sonnet-4-6")
        );
    }
    running.stop(ExitCause::Ended);
    setup.assert_cleaned(0);
}

#[test]
fn one_group_hire_preserves_configured_order_and_primary_after_sorted_leases() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start(true);
    let result = hires.hire(params).unwrap();
    running.stop(ExitCause::Ended);
    let records = setup.fixture.records();
    let leased: Vec<_> = records
        .iter()
        .filter_map(Record::worktree_lease)
        .map(|lease| {
            assert!(lease.identity.is_some());
            lease.origin.file_name().and_then(OsStr::to_str).unwrap()
        })
        .collect();
    assert_eq!(leased, ["api", "docs", "web"]);
    let hires: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::AgentHiredWithRepositories(hire) => Some(hire),
            _ => None,
        })
        .collect();
    assert_eq!(hires.len(), 1);
    assert_eq!(hires[0].id, result.agent);
    let scope = &hires[0].repositories;
    assert_eq!(
        scope
            .repositories
            .entries()
            .iter()
            .map(|entry| entry.repo_id.as_str())
            .collect::<Vec<_>>(),
        ["web", "api", "docs"]
    );
    assert_eq!(scope.repositories.primary_id(), &repo_id("api"));
    let grants: std::collections::BTreeMap<_, _> = setup
        .fixture
        .entries()
        .into_iter()
        .filter_map(|(lsn, entry)| match entry.record {
            Record::RepositoryTrustSet { repo_id, .. } => Some((repo_id, lsn)),
            _ => None,
        })
        .collect();
    assert!(
        scope
            .repositories
            .entries()
            .iter()
            .all(|entry| entry.value.trust_lsn == grants[&entry.repo_id])
    );
    assert!(
        scope
            .repositories
            .entries()
            .iter()
            .all(|entry| entry.value.execution.directory.exists())
    );
    assert!(setup.host.spawned().is_empty());
}

#[test]
fn refreshing_one_grant_to_identical_pins_aborts_the_whole_pending_group() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start(true);
    let repeat = hires.clone();
    setup.runner.after_add(2, false, move || {
        repeat
            .trust(&dudes_proto::rpc::workspace::TrustParams {
                workspace_id: "service".into(),
                repo_id: Some(repo_id("web")),
                trusted: true,
            })
            .unwrap();
    });
    assert!(hires.hire(params).is_err());
    running.stop(ExitCause::Ended);
    setup.assert_cleaned(2);
}

#[test]
fn a_failure_reported_after_the_later_add_cleans_every_acknowledged_lease() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start(true);
    setup.runner.after_add(3, true, || {});
    assert!(hires.hire(params).is_err());
    running.stop(ExitCause::Ended);
    setup.assert_cleaned(3);
}

#[test]
fn pin_drift_in_an_unleased_member_aborts_every_pending_copy() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start(true);
    let origin = setup
        .roots
        .path()
        .canonicalize()
        .unwrap()
        .join("web/.git/config");
    setup.runner.after_add(2, false, move || {
        use std::io::Write;
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(origin)
            .unwrap();
        config
            .write_all(b"\n[core]\n  ignorecase = true\n")
            .unwrap();
    });
    assert!(hires.hire(params).is_err());
    running.stop(ExitCause::Ended);
    setup.assert_cleaned(2);
}

#[test]
fn an_applied_project_removal_aborts_every_pending_copy() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start(true);
    let handle = running.handle.clone();
    let config = setup.settings.operator_config.clone().unwrap();
    setup.runner.after_add(2, false, move || {
        use dudes_proto::rpc::floor::EnrollmentApply;
        let original = crate::config::load_operator(&config).unwrap();
        let mut candidate = original;
        candidate.enrollment.profiles.clear();
        let sha256 = "0".repeat(64);
        let read = crate::enrollment_reload::ReloadRead {
            seq: 1,
            sha256: sha256.clone(),
            path: config,
            config: candidate,
            memory_config: dudes_journal::MemoryCondenseConfig::default(),
            apply: Some(EnrollmentApply {
                sha256,
                added: vec![],
                removed: vec!["service".into()],
                changed: vec![],
            }),
            pid: None,
        };
        assert!(handle.enrollment_reload(read).unwrap().applied);
    });
    assert!(hires.hire(params).is_err());
    running.stop(ExitCause::Ended);
    setup.assert_cleaned(2);
}

#[test]
fn a_known_failed_final_append_cleans_every_pending_copy() {
    let setup = Setup::new();
    let daemon = setup.boot(true);
    let target = daemon.repository_hire_target("service").unwrap();
    let failure = daemon.append_failure();
    let mut running = common::start(daemon);
    let hires = WorktreeHires::new(running.handle.clone(), setup.git.clone());
    for name in ["web", "api", "docs"] {
        hires
            .trust(&dudes_proto::rpc::workspace::TrustParams {
                workspace_id: "service".into(),
                repo_id: Some(repo_id(name)),
                trusted: true,
            })
            .unwrap();
    }
    let mut params = common::hire_params(Path::new(""));
    params.model = Some(dudes_types::ModelName::parse("claude-sonnet-4-6").unwrap());
    params.project = Some(target);
    params.trusted = true;
    params.checkout = Some(Checkout::Worktree);
    setup.runner.after_add(3, false, move || {
        failure.fail_nth(std::num::NonZeroUsize::new(1).unwrap());
    });
    assert!(hires.hire(params).is_err());
    running.stop(ExitCause::Ended);
    setup.assert_cleaned(3);
}

fn uncertain_final_hire(point: OnboardingFailurePoint) {
    let setup = Setup::new();
    let daemon = setup.boot(true);
    daemon.ctx.onboarding_failure.arm(point);
    let target = daemon.repository_hire_target("service").unwrap();
    let mut running = common::start(daemon);
    let hires = WorktreeHires::new(running.handle.clone(), setup.git.clone());
    for name in ["web", "api", "docs"] {
        hires
            .trust(&dudes_proto::rpc::workspace::TrustParams {
                workspace_id: "service".into(),
                repo_id: Some(repo_id(name)),
                trusted: true,
            })
            .unwrap();
    }
    let mut params = common::hire_params(Path::new(""));
    params.model = Some(dudes_types::ModelName::parse("claude-sonnet-4-6").unwrap());
    params.project = Some(target);
    params.trusted = true;
    params.checkout = Some(Checkout::Worktree);
    assert!(hires.hire(params).is_err());
    running.stop(ExitCause::Ended);
    assert!(
        setup
            .fixture
            .records()
            .iter()
            .all(|record| !matches!(record, Record::WorktreeRemoved { .. }))
    );
    let daemon = setup.boot(false);
    let row = daemon.world().agents.values().next().unwrap();
    assert_eq!(row.repositories.worktrees().count(), 3);
    for lease in daemon.world().worktrees.values() {
        assert!(lease.path.exists());
        assert!(lease.agent.is_some());
    }
    let mut running = common::start(daemon);
    running.stop(ExitCause::Ended);
    assert!(setup.host.spawned().is_empty());
    assert_eq!(
        setup
            .fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::AgentHiredWithRepositories(_)))
            .count(),
        1
    );
}

#[test]
fn an_uncertain_final_hire_retains_every_copy_and_replay_owns_them() {
    uncertain_final_hire(OnboardingFailurePoint::Hire);
}

#[test]
fn projection_failure_after_a_group_hire_retains_every_copy_for_replay() {
    uncertain_final_hire(OnboardingFailurePoint::HireProjection);
}

#[test]
fn a_crash_prefix_between_leases_cleans_every_pending_member_on_boot() {
    let setup = Setup::new();
    let (mut running, _, params) = setup.start(true);
    let Answered::RepositoryHireAdmitted(plan) = running
        .handle
        .ask(Ask::RepositoryHireAdmit(Box::new(params)))
        .unwrap()
    else {
        panic!("group admission")
    };
    for name in ["api", "docs"] {
        let origin = &plan
            .repositories
            .get(&repo_id(name))
            .unwrap()
            .origin
            .directory;
        let base = setup.git.commit(origin, "HEAD").unwrap();
        let Answered::WorktreeLeased(lease) = running
            .handle
            .ask(Ask::RepositoryHireLease {
                plan: plan.clone(),
                repo_id: repo_id(name),
                base,
            })
            .unwrap()
        else {
            panic!("lease")
        };
        setup
            .git
            .add(&lease.origin, &lease.path, &lease.branch, lease.base)
            .unwrap();
    }
    running.stop(ExitCause::Ended);
    let daemon = setup.boot(true);
    let target = daemon.repository_hire_target("service").unwrap();
    let mut running = common::start(daemon);
    let deadline = Instant::now() + Duration::from_secs(3);
    while setup
        .fixture
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
        .count()
        != 2
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    running.stop(ExitCause::Ended);
    setup.assert_cleaned(2);
    let old: std::collections::BTreeSet<_> = setup
        .fixture
        .records()
        .iter()
        .filter_map(Record::worktree_lease)
        .map(|lease| lease.worktree)
        .collect();
    let daemon = setup.boot(true);
    let mut running = common::start(daemon);
    let hires = WorktreeHires::new(running.handle.clone(), setup.git.clone());
    let mut retry = common::hire_params(Path::new(""));
    retry.model = Some(dudes_types::ModelName::parse("claude-sonnet-4-6").unwrap());
    retry.project = Some(target);
    retry.checkout = Some(Checkout::Worktree);
    retry.trusted = true;
    hires.hire(retry).unwrap();
    running.stop(ExitCause::Ended);
    let records = setup.fixture.records();
    let leases: Vec<_> = records.iter().filter_map(Record::worktree_lease).collect();
    assert_eq!(leases.len(), 5);
    assert!(
        leases[2..]
            .iter()
            .all(|lease| !old.contains(&lease.worktree)
                && lease.identity.is_some()
                && lease.path.exists())
    );
}

#[test]
fn a_missing_nonprimary_live_copy_holds_the_whole_replayed_hire() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start(true);
    let result = hires.hire(params).unwrap();
    running.stop(ExitCause::Ended);
    let copies: Vec<_> = setup
        .fixture
        .records()
        .iter()
        .filter_map(Record::worktree_lease)
        .map(|lease| {
            assert!(lease.identity.is_some());
            lease.path.to_path_buf()
        })
        .collect();
    let missing = &copies[1];
    std::fs::rename(missing, missing.with_extension("moved")).unwrap();
    let mut running = common::start(setup.boot(false));
    running.stop(ExitCause::Ended);
    assert!(setup.fixture.records().iter().any(|record| matches!(record, Record::AgentHeld { agent, cause: dudes_proto::AutomaticHold::WorktreeMissing { .. }, .. } if *agent == result.agent)));
    assert!(!missing.exists());
    assert!(
        copies
            .iter()
            .filter(|path| *path != missing)
            .all(|path| path.exists())
    );
    assert!(setup.host.spawned().is_empty());
}

fn lease_entry(prepared: &PreparedWorktreeLease) -> Entry {
    let lease = &prepared.lease;
    Entry {
        at: dudes_types::Timestamp::from_unix_millis(1_787_394_600_000),
        record: Record::WorktreeLeasedWithIdentity {
            worktree: lease.worktree,
            origin: lease.origin.clone(),
            path: lease.path.clone(),
            branch: lease.branch.clone(),
            base: lease.base,
            from: prepared.from,
            identity: lease.identity.clone().unwrap(),
        },
    }
}

#[test]
fn exact_group_lease_context_refuses_stale_member_base_id_and_identity() {
    let setup = Setup::new();
    let (mut daemon, plan) = setup.admitted();
    let member_id = repo_id("api");
    let origin = plan
        .repositories
        .get(&member_id)
        .unwrap()
        .origin
        .directory
        .clone();
    let base = setup.git.commit(&origin, "HEAD").unwrap();
    let prepared = daemon.prepare_worktree_lease(origin, base, None).unwrap();
    let admitted = repository_hire::RepositoryLeaseAppend {
        plan,
        repo_id: member_id,
        prepared: prepared.clone(),
    };
    let valid = lease_entry(&prepared);
    let mut cases = Vec::new();
    let mut stale = admitted.clone();
    stale.plan.catalog_lsn = Lsn(stale.plan.catalog_lsn.0 + 1);
    cases.push((stale, valid.clone()));
    let mut member = admitted.clone();
    member.repo_id = repo_id("web");
    cases.push((member, valid.clone()));
    for field in [
        "base", "id", "identity", "from", "path", "branch", "legacy", "record",
    ] {
        let mut entry = valid.clone();
        if let Record::WorktreeLeasedWithIdentity {
            worktree,
            base,
            identity,
            from,
            path,
            branch,
            origin,
        } = &mut entry.record
        {
            match field {
                "base" => *base = dudes_types::GitCommit::parse(&"f".repeat(40)).unwrap(),
                "id" => *worktree = dudes_types::WorktreeId::from_bytes([0; 16]),
                "identity" => identity.root.inode += 1,
                "from" => *from = Some(dudes_types::WorktreeId::from_bytes([0; 16])),
                "path" => *path = path.with_extension("other"),
                "branch" => branch.push_str("-other"),
                "legacy" => {
                    entry.record = Record::WorktreeLeased {
                        worktree: *worktree,
                        origin: origin.clone(),
                        path: path.clone(),
                        branch: branch.clone(),
                        base: *base,
                        from: *from,
                    }
                }
                "record" => {
                    entry.record = Record::AgentRetired {
                        id: AgentId::NIL,
                        reason: "test".into(),
                        browser_retire: None,
                    }
                }
                _ => unreachable!(),
            }
        }
        cases.push((admitted.clone(), entry));
    }
    for (context, entry) in cases {
        daemon.ctx.repository_lease_append = Some(context);
        assert!(
            daemon
                .ctx
                .consume_repository_lease(daemon.sup.world(), &entry)
                .is_err()
        );
        assert!(daemon.ctx.repository_lease_append.is_none());
    }
    assert!(
        setup
            .fixture
            .records()
            .iter()
            .all(|record| record.worktree_lease().is_none())
    );
}

#[test]
fn group_lease_context_is_consumed_once_and_refuses_nesting() {
    let setup = Setup::new();
    let (mut daemon, plan) = setup.admitted();
    let member = repo_id("api");
    let origin = plan
        .repositories
        .get(&member)
        .unwrap()
        .origin
        .directory
        .clone();
    let base = setup.git.commit(&origin, "HEAD").unwrap();
    let prepared = daemon.prepare_worktree_lease(origin, base, None).unwrap();
    let admitted = repository_hire::RepositoryLeaseAppend {
        plan: plan.clone(),
        repo_id: member.clone(),
        prepared: prepared.clone(),
    };
    let entry = lease_entry(&prepared);
    daemon.ctx.repository_lease_append = Some(admitted.clone());
    assert!(
        daemon
            .ctx
            .consume_repository_lease(daemon.sup.world(), &entry)
            .unwrap()
            .is_some()
    );
    assert!(daemon.ctx.repository_lease_append.is_none());
    assert!(daemon.ctx.append(daemon.sup.world(), &entry).is_err());
    daemon.ctx.repository_lease_append = Some(admitted);
    assert!(matches!(
        daemon.repository_hire_lease(&plan, &member, base).unwrap(),
        Answered::RepositoryHireRejected(_)
    ));
    assert!(daemon.ctx.repository_lease_append.is_none());
    assert!(
        setup
            .fixture
            .records()
            .iter()
            .all(|record| record.worktree_lease().is_none())
    );
}

#[test]
fn group_lease_errors_clear_context_before_an_unrelated_retry() {
    let setup = Setup::new();
    let (mut daemon, plan) = setup.admitted();
    let member = repo_id("api");
    let origin = &plan.repositories.get(&member).unwrap().origin.directory;
    let base = setup.git.commit(origin, "HEAD").unwrap();
    let parent = daemon.sup.config().data_dir.join("worktrees");
    if parent.exists() {
        std::fs::remove_dir(&parent).unwrap();
    }
    std::fs::write(&parent, "occupied").unwrap();
    assert!(matches!(
        daemon.repository_hire_lease(&plan, &member, base).unwrap(),
        Answered::RepositoryHireRejected(_)
    ));
    assert!(daemon.ctx.repository_lease_append.is_none());
    std::fs::remove_file(&parent).unwrap();
    let mut stale = plan.clone();
    stale.catalog_lsn = Lsn(stale.catalog_lsn.0 + 1);
    assert!(matches!(
        daemon.repository_hire_lease(&stale, &member, base).unwrap(),
        Answered::RepositoryHireRejected(_)
    ));
    assert!(daemon.ctx.repository_lease_append.is_none());
    daemon
        .append_failure()
        .fail_nth(std::num::NonZeroUsize::new(1).unwrap());
    assert!(matches!(
        daemon.repository_hire_lease(&plan, &member, base).unwrap(),
        Answered::RepositoryHireRejected(_)
    ));
    assert!(daemon.ctx.repository_lease_append.is_none());
    let next = repo_id("docs");
    let base = setup
        .git
        .commit(
            &plan.repositories.get(&next).unwrap().origin.directory,
            "HEAD",
        )
        .unwrap();
    let Answered::WorktreeLeased(lease) = daemon.repository_hire_lease(&plan, &next, base).unwrap()
    else {
        panic!("fresh unrelated lease")
    };
    assert!(lease.identity.is_some());
    assert!(daemon.ctx.repository_lease_append.is_none());
    assert_eq!(
        setup
            .fixture
            .records()
            .iter()
            .filter_map(Record::worktree_lease)
            .count(),
        1
    );
}

#[test]
fn group_lease_root_drift_after_preparation_appends_nothing() {
    let setup = Setup::new();
    let daemon = setup.boot(true);
    let parent = daemon.sup.config().data_dir.join("worktrees");
    let pause = daemon.onboarding_pause();
    pause.arm(OnboardingPausePoint::BeforeWorktreeLeaseAppend);
    let (mut running, hires, params) = setup.start_from(daemon);
    let retry = hires.clone();
    let retry_params = params.clone();
    let worker = std::thread::spawn(move || hires.hire(params));
    assert!(pause.wait_reached(Duration::from_secs(3)));
    let root = std::fs::read_dir(&parent)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(root.join("sentinel"), "keep").unwrap();
    pause.release();
    assert!(worker.join().unwrap().is_err());
    assert_eq!(setup.runner.adds.load(Ordering::SeqCst), 0);
    assert!(
        setup
            .fixture
            .records()
            .iter()
            .all(|record| record.worktree_lease().is_none())
    );
    assert_eq!(
        std::fs::read_to_string(root.join("sentinel")).unwrap(),
        "keep"
    );
    retry.hire(retry_params).unwrap();
    running.stop(ExitCause::Ended);
    assert_eq!(
        std::fs::read_to_string(root.join("sentinel")).unwrap(),
        "keep"
    );
}

#[test]
fn replacement_at_the_final_group_permit_refuses_the_entire_assignment() {
    let setup = Setup::new();
    let daemon = setup.boot(true);
    let pause = daemon.onboarding_pause();
    pause.arm(OnboardingPausePoint::BeforeSocketHireAppend);
    let (mut running, hires, params) = setup.start_from(daemon);
    let worker = std::thread::spawn(move || hires.hire(params));
    assert!(pause.wait_reached(Duration::from_secs(3)));
    let records = setup.fixture.records();
    let leases: Vec<_> = records.iter().filter_map(Record::worktree_lease).collect();
    assert_eq!(leases.len(), 3);
    let root = leases[1].path.to_path_buf();
    let moved = root.with_extension("preserved");
    std::fs::rename(&root, &moved).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("sentinel"), "keep").unwrap();
    pause.release();
    assert!(worker.join().unwrap().is_err());
    running.stop(ExitCause::Ended);
    let records = setup.fixture.records();
    assert!(records.iter().all(|record| record.hire().is_none()));
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                Record::WorktreeRemoved {
                    how: WorktreeRemoval::NeverAssigned,
                    ..
                }
            ))
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(root.join("sentinel")).unwrap(),
        "keep"
    );
    assert!(moved.join("tracked.txt").exists());
}

#[test]
fn an_uncertain_later_lease_retains_acknowledged_copies_for_replay() {
    let setup = Setup::new();
    let daemon = setup.boot(true);
    let failure = daemon.append_failure();
    let (mut running, hires, params) = setup.start_from(daemon);
    setup
        .runner
        .after_add(1, false, move || failure.short_write_next(1));
    assert!(hires.hire(params).is_err());
    running.stop(ExitCause::Ended);
    let records = setup.fixture.records();
    assert!(
        records
            .iter()
            .all(|record| !matches!(record, Record::WorktreeRemoved { .. }))
    );
    assert!(records.iter().all(|record| record.hire().is_none()));
    let leases: Vec<_> = records.iter().filter_map(Record::worktree_lease).collect();
    assert_eq!(leases.len(), 1);
    assert!(leases[0].identity.is_some());
    assert!(leases[0].path.join("tracked.txt").exists());
}

#[test]
fn an_aliased_state_parent_creates_canonical_copies_and_keeps_historical_paths() {
    let mut setup = Setup::new();
    let aliases = tempfile::tempdir().unwrap();
    let alias = aliases.path().join("state");
    let canonical = setup.settings.state_dir.canonicalize().unwrap();
    std::os::unix::fs::symlink(canonical.parent().unwrap(), &alias).unwrap();
    let configured = alias.join(canonical.file_name().unwrap());
    setup.settings.state_dir = configured.clone();
    let origin = setup.roots.path().canonicalize().unwrap().join("api");
    let legacy = dudes_types::WorktreeId::from_bytes([0xa5; 16]);
    let historical_path = configured.join("worktrees").join(legacy.to_string());
    let (mut writer, _) =
        dudes_journal::open(&setup.fixture.journal_dir(), setup.settings.journal).unwrap();
    writer
        .append(&Entry {
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_600_001),
            record: Record::WorktreeLeased {
                worktree: legacy,
                origin: origin.clone(),
                path: historical_path.clone(),
                branch: format!("dudes/{}", legacy.short()),
                base: setup.git.commit(&origin, "HEAD").unwrap(),
                from: None,
            },
        })
        .unwrap();
    drop(writer);
    let daemon = setup.boot(true);
    assert_eq!(daemon.world().worktrees[&legacy].path, historical_path);
    let (mut running, hires, params) = setup.start_from(daemon);
    hires.hire(params).unwrap();
    running.stop(ExitCause::Ended);
    let records = setup.fixture.records();
    let copies: Vec<_> = records
        .iter()
        .filter_map(Record::worktree_lease)
        .filter(|lease| lease.worktree != legacy)
        .collect();
    assert_eq!(copies.len(), 3);
    assert!(copies.iter().all(|lease| lease.identity.is_some()
        && lease.path.starts_with(&canonical)
        && lease.path.canonicalize().unwrap() == lease.path));
    let daemon = setup.boot(false);
    assert_eq!(daemon.world().worktrees[&legacy].path, historical_path);
}

#[test]
fn a_hook_configured_nonprimary_origin_refuses_the_group_before_any_lease() {
    let setup = Setup::new();
    let origin = setup.roots.path().canonicalize().unwrap().join("web");
    command(
        &setup.git,
        &origin,
        &["config", "hook.checkout-probe.command", "true"],
    );
    command(
        &setup.git,
        &origin,
        &["config", "hook.checkout-probe.event", "post-checkout"],
    );
    let (mut running, hires, params) = setup.start(true);
    assert!(setup.fixture.records().iter().any(|record| matches!(record,
        Record::RepositoryTrustSet { repo_id: member, pin: Some(_), .. } if *member == repo_id("web")
    )));
    let result = hires.hire(params);
    running.stop(ExitCause::Ended);
    let error = result.expect_err("configured hooks are refused even when the grant pins them");
    assert!(
        error.message.contains("configuration has `hook.*` keys"),
        "{error:?}"
    );
    assert_eq!(setup.runner.adds.load(Ordering::SeqCst), 0);
    setup.assert_cleaned(0);
}
