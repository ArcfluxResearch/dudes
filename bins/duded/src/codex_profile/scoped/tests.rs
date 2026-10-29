use super::*;
use crate::predicate::PredicateOutcome;
use dudes_proto::instruction::InstructionIssueAssociation;
use dudes_proto::record::{FileIdentity, RecordedWorkspace, TrustedRepository};
use dudes_proto::repository::{
    ExecutionRepository, OwnedRepository, RepoId, RepositoryEntry, RepositoryPolicy, RepositorySet,
    RepositoryTurnGrant, ScopedRepositories,
};
use dudes_providers::adapter::{Adapter as _, AgentCtx, TurnSession};
use dudes_providers::codex::{CodexAdapter, config::CodexConfig};
use dudes_providers::scoped::{ScopedLaunchInputs, ScopedPrivatePaths};
use dudes_types::{AgentId, GitCommit, Mode, ModelSelection, WorktreeId};

#[derive(Default)]
struct Runner {
    calls: Mutex<Vec<LaunchSpec>>,
    captured: Mutex<Option<PredicateCaptured>>,
}

impl crate::predicate::PredicateRunner for Runner {
    fn run(&self, spec: &LaunchSpec, _: std::time::Duration) -> io::Result<PredicateOutcome> {
        self.calls.lock().unwrap().push(spec.clone());
        Ok(PredicateOutcome::Finished {
            exit: Some(0),
            output_tail: "{\"synchronized\":true,\"config\":false,\"requirements\":false}".into(),
            timed_out: false,
        })
    }
    fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: std::time::Duration,
        limit: usize,
    ) -> io::Result<PredicateCaptured> {
        assert_eq!(timeout, PROBE_DEADLINE);
        assert_eq!(limit, CATALOG_LIMIT);
        self.calls.lock().unwrap().push(spec.clone());
        Ok(self.captured.lock().unwrap().take().unwrap())
    }
    fn sweep(&self) {}
}

struct Fixture {
    _root: tempfile::TempDir,
    root: PathBuf,
    profile: Arc<CodexProfile>,
    runner: Arc<Runner>,
    launch: ScopedManagedLaunch,
    bytes: Vec<u8>,
    outbox: Arc<crate::mailbox::OutboxGrant>,
}

impl Fixture {
    fn new(resume: bool) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        let identity = |path: &Path| {
            let metadata = std::fs::metadata(path).unwrap();
            FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        };
        let mut entries = Vec::new();
        let mut git = Vec::new();
        for (index, name) in ["a", "b", "c"].into_iter().enumerate() {
            let origin = path.join(format!("origin-{name}"));
            let common = origin.join(".git");
            let execution = path.join(format!("copy-{name}"));
            let gitdir = common.join("worktrees/copy");
            owner_directory(&origin).unwrap();
            owner_directory(&gitdir).unwrap();
            owner_directory(&execution).unwrap();
            std::fs::write(
                execution.join(".git"),
                format!("gitdir: {}\n", gitdir.display()),
            )
            .unwrap();
            let repo_id = RepoId::parse(name).unwrap();
            git.push(RepositoryEntry {
                repo_id: repo_id.clone(),
                value: common.clone(),
            });
            entries.push(RepositoryEntry {
                repo_id,
                value: OwnedRepository {
                    origin: TrustedRepository {
                        directory: origin.clone(),
                        directory_identity: identity(&origin),
                        common_dir: common.clone(),
                        common_identity: identity(&common),
                    },
                    execution: ExecutionRepository {
                        directory: execution.clone(),
                        directory_identity: identity(&execution),
                        git_dir: gitdir.clone(),
                        git_identity: identity(&gitdir),
                    },
                    worktree: WorktreeId::from_bytes([u8::try_from(index).unwrap(); 16]),
                    base: GitCommit::parse(&"a".repeat(40)).unwrap(),
                    trust_lsn: Lsn::from_parts(0, 1).unwrap(),
                },
            });
        }
        let hired = ScopedRepositories {
            project: RecordedWorkspace {
                id: "service".into(),
                label: "Service".into(),
            },
            catalog_lsn: Lsn::from_parts(0, 2).unwrap(),
            policy: RepositoryPolicy::WorkingTreeV1,
            repositories: RepositorySet::new(RepoId::parse("a").unwrap(), entries.clone()).unwrap(),
        };
        let selected = if resume {
            vec![entries[1].clone()]
        } else {
            vec![entries[1].clone(), entries[0].clone()]
        };
        let grant = RepositoryTurnGrant {
            hired_lsn: Lsn::from_parts(0, 3).unwrap(),
            project: "service".into(),
            catalog_lsn: hired.catalog_lsn,
            policy: hired.policy,
            repositories: RepositorySet::new(RepoId::parse("b").unwrap(), selected).unwrap(),
            ticket: None,
        };
        let bytes = serde_json::to_vec(&serde_json::json!({"models":[{"slug":"gpt-5.5", "shell_type":"unified_exec", "apply_patch_tool_type":"freeform", "node_repl_disabled":false, "node_repl_auto_review_required":false, "experimental_supported_tools":[], "tool_mode":null, "supports_search_tool":true,"web_search_tool_type":"text_and_image"}]})).unwrap();
        let runner = Arc::new(Runner::default());
        *runner.captured.lock().unwrap() = Some(PredicateCaptured::Finished {
            exit: Some(0),
            stdout: bytes.clone(),
            truncated: false,
            timed_out: false,
        });
        let mut profile = CodexProfile::new(&path.join("state"), &[], runner.clone());
        Arc::get_mut(&mut profile)
            .unwrap()
            .scoped_test_qualification = Some((digest(b"binary"), digest(&bytes), bytes.len()));
        let agent_dir = path
            .join("state/agents")
            .join(AgentId::from_parts(1, 1).to_string());
        let outbox = crate::mailbox::outbox_fixture(&path.join("state"), AgentId::from_parts(1, 1));
        let private = ScopedPrivatePaths {
            home: agent_dir.join("home"),
            profile: profile.home.clone(),
            work: agent_dir.join("work"),
            outbox: agent_dir.join("outbox"),
            temp: agent_dir.join("tmp"),
        };
        let program = path.join("codex");
        std::fs::write(&program, b"binary").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let adapter = CodexAdapter::new(CodexConfig {
            streaming: true,
            command: program.to_string_lossy().into_owned(),
            ..CodexConfig::default()
        })
        .with_managed_home(profile.home.clone());
        let ctx = AgentCtx {
            agent: AgentId::from_parts(1, 1),
            cwd: &grant.repositories.primary().execution.directory,
            agent_dir: &agent_dir,
            daemon_env: &[],
            sandbox: Some(true),
            trusted: true,
            max_turns: Some(8),
            max_budget: None,
            mcp_servers: &[],
        };
        let session = if resume {
            TurnSession::Resume(dudes_types::SessionId::new(
                "00000000-0000-7000-8000-000000000002",
            ))
        } else {
            TurnSession::Assigned
        };
        let launch = adapter
            .managed_scoped(
                &ctx,
                &ScopedLaunchInputs {
                    grant: &grant,
                    hired: &hired,
                    origin_git_dirs: &git,
                    private,
                    mode: Mode::Managed,
                    platform: ScopedPlatform::MacOsArm64,
                },
                "exact immutable task",
                &session,
                &ModelSelection::named("gpt-5.5"),
            )
            .unwrap();
        Self {
            _root: root,
            root: path,
            profile,
            runner,
            launch,
            bytes,
            outbox,
        }
    }

    fn attempt() -> Attempt {
        let turn = TurnId::from_parts(1, 1);
        Attempt {
            turn,
            generation: 7,
            started: Lsn::from_parts(0, 4).unwrap(),
            owner: InstructionObservationOwner::Managed {
                agent: AgentId::from_parts(1, 1),
                turn,
                issue: InstructionIssueAssociation::Issued {
                    issue_lsn: Lsn::from_parts(0, 3).unwrap(),
                },
            },
        }
    }

    fn prepare(&self) -> io::Result<PreparedScopedLaunch> {
        self.profile
            .prepare_scoped(self.launch.clone(), Self::attempt())
    }
}

#[test]
fn the_existing_worker_pool_returns_one_private_bundle_without_browser_input() {
    let f = Fixture::new(true);
    let (jobs, completed) = std::sync::mpsc::channel();
    let handle = crate::ports::Handle::new(jobs);
    let pause = crate::chat_launch::BrowserLaunchPause::default();
    pause.arm();
    let mut workers = crate::chat_launch::LaunchPreparations::new(&handle, pause.clone()).unwrap();
    assert!(
        workers.prepare_scoped(crate::chat_launch::ScopedPreparation {
            attempt: Fixture::attempt(),
            launch: f.launch.clone(),
            profile: Arc::clone(&f.profile),
            outbox: Arc::clone(&f.outbox),
        })
    );
    assert!(pause.wait(std::time::Duration::from_secs(1)));
    assert!(completed.try_recv().is_err());
    pause.release();
    let crate::ports::Job::ScopedLaunchPrepared(completion) = completed
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap()
    else {
        panic!("private worker completion");
    };
    assert_eq!(completion.attempt, Fixture::attempt());
    let prepared = completion.prepared.unwrap();
    assert_eq!(prepared.launch.inventory, f.launch.inventory);
    assert_eq!(prepared.launch.install, f.launch.install);
    assert_eq!(
        completion.outbox.path(),
        prepared.launch.inventory.private.outbox
    );
    let LaunchInput::Protocol(startup) = &prepared.launch.spec.stdin else {
        panic!("protocol");
    };
    prepared.binding.before_task(startup).unwrap();
    workers.shutdown();
}

#[test]
fn a_missing_private_owner_and_widened_inventory_are_refused() {
    let f = Fixture::new(false);
    let mut widened = f.launch.clone();
    widened
        .inventory
        .writable
        .push(dudes_providers::scoped::WritableLocation {
            purpose: dudes_providers::scoped::WritablePurpose::Repository(
                RepoId::parse("c").unwrap(),
            ),
            path: f.root.join("copy-c"),
        });
    assert!(
        f.profile
            .prepare_scoped(widened, Fixture::attempt())
            .is_err()
    );
    let mut spec = f.launch.spec.clone();
    let LaunchInput::Protocol(startup) = spec.stdin.clone() else {
        panic!("protocol");
    };
    assert!(f.profile.prepare(&mut spec, &startup).is_err());
}

#[test]
fn assigned_and_resume_keep_the_exact_narrowed_roots_and_one_catalog() {
    for resume in [false, true] {
        let f = Fixture::new(resume);
        let prepared = f.prepare().unwrap();
        assert_eq!(prepared.attempt, Fixture::attempt());
        assert_eq!(prepared.launch.inventory, f.launch.inventory);
        assert_eq!(prepared.launch.install, f.launch.install);
        let LaunchInput::Protocol(startup) = &prepared.launch.spec.stdin else {
            panic!("protocol");
        };
        assert_eq!(
            startup.scoped_roots(),
            Some(f.launch.inventory.additional_roots().as_slice())
        );
        assert_eq!(startup.cwd(), f.root.join("copy-b"));
        assert!(
            !startup
                .scoped_roots()
                .unwrap()
                .contains(&f.root.join("copy-c"))
        );
        if resume {
            assert!(
                !startup
                    .scoped_roots()
                    .unwrap()
                    .contains(&f.root.join("copy-a"))
            );
        }
        let catalogs = prepared
            .launch
            .spec
            .argv
            .iter()
            .filter(|arg| {
                arg.to_str()
                    .is_some_and(|arg| arg.starts_with("model_catalog_json="))
            })
            .count();
        assert_eq!(catalogs, 1);
        prepared.binding.before_task(startup).unwrap();
        let catalog = f
            .profile
            .home
            .join(format!("catalogs/{}.json", digest(&f.bytes)));
        assert_eq!(std::fs::read(catalog).unwrap(), f.bytes);
        let calls = f.runner.calls.lock().unwrap();
        let export = calls
            .iter()
            .find(|spec| spec.argv.last().is_some_and(|arg| arg == "--bundled"))
            .unwrap();
        assert!(
            export
                .env
                .iter()
                .all(|var| !var.name().to_string_lossy().contains("KEY")
                    && !var.name().to_string_lossy().starts_with("DUDES_"))
        );
        assert!(export.argv.iter().any(|arg| {
            arg.to_str()
                .is_some_and(|arg| arg.contains("deny network*"))
        }));
    }
}

#[test]
fn poisoned_home_refuses_before_any_probe_or_install() {
    for name in [".env", "config.toml", "rules", "profiles", "hooks.json"] {
        let f = Fixture::new(false);
        owner_directory(&f.profile.home).unwrap();
        std::fs::write(f.profile.home.join(name), b"poison").unwrap();
        assert!(f.prepare().is_err());
        assert!(f.runner.calls.lock().unwrap().is_empty());
        assert!(!f.launch.inventory.private.work.exists());
    }
}

#[test]
fn catalog_truncation_deadline_and_drift_refuse() {
    for (truncated, timed_out, bytes) in [
        (true, false, None),
        (false, true, None),
        (false, false, Some(b"wrong".to_vec())),
    ] {
        let f = Fixture::new(false);
        *f.runner.captured.lock().unwrap() = Some(PredicateCaptured::Finished {
            exit: Some(0),
            stdout: bytes.unwrap_or_else(|| f.bytes.clone()),
            truncated,
            timed_out,
        });
        assert!(f.prepare().is_err());
    }
    let f = Fixture::new(false);
    std::fs::write(&f.root.join("codex"), b"changed binary").unwrap();
    assert!(f.prepare().is_err());
    assert!(
        !f.runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|spec| spec.argv.last().is_some_and(|arg| arg == "--bundled"))
    );
}

#[test]
fn retained_binding_refuses_catalog_profile_and_mutation_generation_changes() {
    for change in 0..3 {
        let f = Fixture::new(false);
        let prepared = f.prepare().unwrap();
        let LaunchInput::Protocol(startup) = &prepared.launch.spec.stdin else {
            panic!("protocol");
        };
        match change {
            0 => {
                let path = f
                    .profile
                    .home
                    .join(format!("catalogs/{}.json", digest(&f.bytes)));
                std::fs::remove_file(&path).unwrap();
                std::fs::write(path, &f.bytes).unwrap();
            }
            1 => {
                std::fs::rename(&f.profile.home, f.root.join("old-profile")).unwrap();
                owner_directory(&f.profile.home).unwrap();
            }
            _ => {
                drop(f.profile.mutations.mutate().unwrap());
            }
        }
        assert!(prepared.binding.before_task(startup).is_err());
    }
}

#[test]
fn existing_aliased_or_mismatched_catalog_is_never_rewritten() {
    for alias in [false, true] {
        let f = Fixture::new(false);
        let directory = f.profile.home.join("catalogs");
        owner_directory(&directory).unwrap();
        let path = directory.join(format!("{}.json", digest(&f.bytes)));
        if alias {
            let other = f.root.join("other");
            std::fs::write(&other, &f.bytes).unwrap();
            std::os::unix::fs::symlink(other, &path).unwrap();
        } else {
            std::fs::write(&path, b"wrong").unwrap();
        }
        assert!(f.prepare().is_err());
        assert!(
            !f.runner
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|spec| spec.argv.last().is_some_and(|arg| arg == "--bundled"))
        );
    }
}
