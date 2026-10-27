use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dudes_config::workspace::{NewWorkspace, RepositoryDefinition};
use dudes_host::{OneShot, OneShotOutcome};
use dudes_proto::repository::{RepositoryEntry, RepositorySet};
use dudes_proto::rpc::agent::{Checkout, HireParams};
use dudes_proto::{Record, WorktreeRemoval};
use dudes_providers::launch::LaunchSpec;
use dudes_types::Provider;

use super::work_test_support as common;
use super::*;
use crate::predicate::{PredicateOutcome, PredicateRunner};
use crate::worktree::git::Git;
use crate::worktree::hire::WorktreeHires;

type AddCut = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Runner {
    inner: OneShot,
    adds: AtomicUsize,
    cut: Mutex<Option<(usize, bool, AddCut)>>,
}

impl Runner {
    fn after_add(&self, nth: usize, fail: bool, cut: impl FnOnce() + Send + 'static) {
        *self.cut.lock().unwrap() = Some((nth, fail, Box::new(cut)));
    }
}

impl PredicateRunner for Runner {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<PredicateOutcome> {
        let result = self.inner.run(spec, timeout)?;
        if spec
            .argv
            .windows(2)
            .any(|args| args == [OsStr::new("worktree"), OsStr::new("add")])
        {
            let nth = self.adds.fetch_add(1, Ordering::SeqCst) + 1;
            let cut = {
                let mut cut = self.cut.lock().unwrap();
                if cut.as_ref().is_some_and(|(at, _, _)| *at == nth) {
                    cut.take()
                } else {
                    None
                }
            };
            if let Some((_, fail, cut)) = cut {
                cut();
                if fail {
                    return Ok(PredicateOutcome::Finished {
                        exit: Some(1),
                        output_tail: "injected failure after partial add".into(),
                        timed_out: false,
                    });
                }
            }
        }
        Ok(match result {
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
        self.inner.sweep();
    }

    fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        cap: usize,
    ) -> io::Result<crate::predicate::PredicateCaptured> {
        self.inner
            .run_captured(spec, timeout, cap)
            .map(|result| match result {
                dudes_host::CapturedOutcome::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                } => crate::predicate::PredicateCaptured::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                },
                dudes_host::CapturedOutcome::Swept => crate::predicate::PredicateCaptured::Swept,
            })
    }
}

struct Setup {
    fixture: common::Fixture,
    roots: tempfile::TempDir,
    settings: crate::Settings,
    runner: Arc<Runner>,
    git: Git,
    host: common::SharedHost,
}

impl Setup {
    fn new() -> Self {
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
        for name in ["web", "api", "docs"] {
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
        settings.predicate_path = std::env::var_os("PATH");
        settings.enrollment = crate::config::load_operator(&config).unwrap().enrollment;
        settings.operator_config = Some(config);
        Self {
            fixture,
            roots,
            settings,
            runner,
            git,
            host: common::SharedHost::new(),
        }
    }

    fn boot(
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

    fn start(&self, capability: bool) -> (common::Running, WorktreeHires, HireParams) {
        let daemon = self.boot(capability);
        let target = daemon.repository_hire_target("service").unwrap();
        let running = common::start(daemon);
        let hires = WorktreeHires::new(running.handle.clone(), self.git.clone());
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
        params.project = Some(target);
        params.checkout = Some(Checkout::Worktree);
        params.trusted = true;
        (running, hires, params)
    }

    fn assert_cleaned(&self, count: usize) {
        let records = self.fixture.records();
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, Record::WorktreeLeased { .. }))
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
            if let Record::WorktreeLeased { path, .. } = record {
                assert!(!path.exists());
            }
            assert!(record.hire().is_none());
        }
        assert!(self.host.spawned().is_empty());
    }
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
fn one_group_hire_preserves_configured_order_and_primary_after_sorted_leases() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start(true);
    let hired = hires.hire(params).unwrap();
    running.stop(ExitCause::Ended);
    let records = setup.fixture.records();
    let leased: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::WorktreeLeased { origin, .. } => origin.file_name().and_then(OsStr::to_str),
            _ => None,
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
    assert_eq!(hires[0].id, hired.agent);
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
fn a_later_partial_add_failure_cleans_every_acknowledged_lease() {
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
fn current_config_invalidation_aborts_every_pending_copy() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start(true);
    let handle = running.handle.clone();
    setup.runner.after_add(2, false, move || {
        handle.enrollment_invalid(u64::MAX).unwrap();
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

#[test]
fn an_uncertain_final_hire_retains_every_copy_and_replay_owns_them() {
    let setup = Setup::new();
    let daemon = setup.boot(true);
    daemon
        .ctx
        .onboarding_failure
        .arm(OnboardingFailurePoint::Hire);
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
    let daemon = setup.boot(false);
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
}

#[test]
fn a_missing_nonprimary_live_copy_holds_the_whole_replayed_hire() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start(true);
    let hired = hires.hire(params).unwrap();
    running.stop(ExitCause::Ended);
    let copies: Vec<_> = setup
        .fixture
        .records()
        .iter()
        .filter_map(|record| match record {
            Record::WorktreeLeased { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    let missing = &copies[1];
    std::fs::rename(missing, missing.with_extension("moved")).unwrap();
    let mut running = common::start(setup.boot(false));
    running.stop(ExitCause::Ended);
    assert!(setup.fixture.records().iter().any(|record| matches!(record, Record::AgentHeld { agent, cause: dudes_proto::AutomaticHold::WorktreeMissing { .. }, .. } if *agent == hired.agent)));
    assert!(!missing.exists());
    assert!(
        copies
            .iter()
            .filter(|path| *path != missing)
            .all(|path| path.exists())
    );
    assert!(setup.host.spawned().is_empty());
}
