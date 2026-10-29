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
    #[expect(
        clippy::too_many_lines,
        reason = "the fixture creates one complete frozen hire and candidate from current filesystem identities"
    )]
    fn new(resume: bool) -> Self {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
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
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let adapter = CodexAdapter::new(CodexConfig {
            streaming: true,
            command: program.to_string_lossy().into_owned(),
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
    assert!(
        !workers.prepare_scoped(crate::chat_launch::ScopedPreparation {
            attempt: Fixture::attempt(),
            launch: f.launch.clone(),
            profile: Arc::clone(&f.profile),
            outbox: Arc::clone(&f.outbox),
        })
    );
    workers.finish_scoped(completion.attempt);
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
fn poisoned_private_environment_refuses_before_any_probe_or_install() {
    let f = Fixture::new(false);
    let mut launch = f.launch.clone();
    launch
        .spec
        .env
        .push(EnvVar::new("CODEX_HOME", f.root.join("other-profile")));
    assert!(
        f.profile
            .prepare_scoped(launch, Fixture::attempt())
            .is_err()
    );
    assert!(f.runner.calls.lock().unwrap().is_empty());
    assert!(!f.profile.home.exists());
}

#[test]
fn only_exact_owned_file_mail_environment_is_retained() {
    let f = Fixture::new(false);
    validate_environment(&f.launch).unwrap();
    for (name, wrong) in [
        ("DUDES_MAILBOX", f.root.join("other-agent").into_os_string()),
        ("DUDES_COMMAND_TRANSPORT", "socket".into()),
    ] {
        for change in 0..3 {
            let mut launch = f.launch.clone();
            let original = launch
                .spec
                .env
                .iter()
                .find(|var| var.name() == name)
                .unwrap()
                .clone();
            launch.spec.env.retain(|var| var.name() != name);
            match change {
                0 => launch.spec.env.push(EnvVar::new(name, &wrong)),
                1 => launch.spec.env.extend([original.clone(), original]),
                _ => {}
            }
            assert!(
                f.profile
                    .prepare_scoped(launch, Fixture::attempt())
                    .is_err()
            );
        }
    }
    for name in ["DUDES_TOKEN", "DUDES_SOCK", "DUDES_RUNTIME_DIR", "UNOWNED"] {
        let mut launch = f.launch.clone();
        launch.spec.env.push(EnvVar::new(name, "ambient"));
        assert!(
            f.profile
                .prepare_scoped(launch, Fixture::attempt())
                .is_err()
        );
    }
    assert!(f.runner.calls.lock().unwrap().is_empty());
    assert!(!f.profile.home.exists());
}

#[test]
fn the_owned_child_checks_the_same_binding_before_enqueuing_its_one_task() {
    use dudes_providers::protocol::ProtocolAction;
    #[derive(Debug)]
    struct Input(Arc<Mutex<Vec<ProtocolAction>>>);
    impl crate::children::ChildInput for Input {
        fn send(&self, action: ProtocolAction) -> io::Result<()> {
            self.0.lock().unwrap().push(action);
            Ok(())
        }
    }
    for change in 0..6 {
        let f = Fixture::new(false);
        let prepared = f.prepare().unwrap();
        let LaunchInput::Protocol(startup) = &prepared.launch.spec.stdin else {
            panic!("protocol");
        };
        let ends = crate::children::ChildEnds::new();
        let (sender, _jobs) = std::sync::mpsc::channel();
        let handle = crate::ports::Handle::new(sender);
        ends.arm(handle.clone());
        ends.configure_codex_profile(Arc::clone(&f.profile));
        ends.prepare_scoped_launch(&prepared, Arc::clone(&f.outbox))
            .unwrap();
        ends.read_next_for(
            Some(Box::new(
                dudes_providers::codex::app_server::AppServerReader::new(startup.clone()),
            )),
            &prepared.launch.spec,
            None,
        );
        let captured = Arc::new(Mutex::new(Vec::new()));
        let key = dudes_supervisor::host::AdoptionKey {
            pid: 4242,
            start_time: 900,
            boot: Some("test".into()),
        };
        let mut child = ends.watch_protocol(&key, Box::new(Input(Arc::clone(&captured))));
        child.line(&serde_json::json!({"id":1,"result":{"userAgent":"dudes/0.164.0-alpha.11.1 (test)","codexHome":startup.home()}}).to_string());
        match change {
            1 => mailbox_during_startup(&f, MailboxActivity::Incoming),
            2 => {
                handle.fence_journal();
            }
            3 => mailbox_during_startup(&f, MailboxActivity::NoMail),
            4 => mailbox_during_startup(&f, MailboxActivity::Outgoing),
            5 => {
                let outbox = &f.launch.inventory.private.outbox;
                std::fs::rename(outbox, f.root.join("retained-outbox")).unwrap();
                owner_directory(outbox).unwrap();
            }
            _ => {}
        }
        child.line(&serde_json::json!({"id":2,"result":{"thread":{"id":"00000000-0000-7000-8000-000000000002","status":{"type":"idle"}},"cwd":startup.cwd(),"model":"gpt-5.5","runtimeWorkspaceRoots":prepared.launch.inventory.writable_roots(),"approvalPolicy":"never","sandbox":{"type":"workspaceWrite","writableRoots":prepared.launch.inventory.additional_roots(),"networkAccess":false,"excludeTmpdirEnvVar":true,"excludeSlashTmp":true}}}).to_string());
        let actions = captured.lock().unwrap();
        let tasks: Vec<_> = actions
            .iter()
            .filter_map(|action| match action {
                ProtocolAction::Write(frame) if frame.is_task() => Some(frame),
                _ => None,
            })
            .collect();
        let delivered = matches!(change, 0 | 1 | 3);
        assert_eq!(tasks.len(), usize::from(delivered));
        if delivered {
            let task: serde_json::Value = serde_json::from_slice(tasks[0].bytes()).unwrap();
            assert_eq!(task["params"]["input"][0]["text"], "exact immutable task");
            assert_eq!(task["method"], "turn/start");
        } else {
            assert_eq!(actions.last(), Some(&ProtocolAction::Fail));
        }
    }
}

#[derive(Clone, Copy)]
enum MailboxActivity {
    NoMail,
    Incoming,
    Outgoing,
}

fn mailbox_during_startup(f: &Fixture, activity: MailboxActivity) {
    use dudes_proto::{Entry, MailAct, MailFrom, MailTo, Record};
    let agent = AgentId::from_parts(1, 1);
    let other = AgentId::from_parts(1, 2);
    let journal = f.root.join("mail-journal");
    let (mut writer, _) =
        dudes_journal::open(&journal, dudes_journal::JournalOptions::default()).unwrap();
    for id in [agent, other] {
        let record: Record = serde_json::from_value(serde_json::json!({"t":"agent_hired", "id":id, "name":"agent", "role":"worker", "department":"test", "provider":"codex", "cwd":f.root, "budget":{"usd_per_day":null,"tokens_per_day":null}})).unwrap();
        writer
            .append(&Entry {
                at: dudes_types::Timestamp::from_unix_millis(1),
                record,
            })
            .unwrap();
    }
    let mut mailbox = crate::mailbox::Mailbox::replay(
        f.root.join("state"),
        &dudes_journal::Reader::new(&journal),
    )
    .unwrap();
    mailbox.coordinate_private_roots(f.profile.private_roots());
    if matches!(activity, MailboxActivity::Incoming) {
        let entry = Entry {
            at: dudes_types::Timestamp::from_unix_millis(2),
            record: Record::Mail {
                id: dudes_types::MailId::from_parts(1, 3),
                from: MailFrom::Human,
                to: MailTo::Agent { agent },
                act: MailAct::Message,
                subject: "message".into(),
                subject_admission: Some(1),
                body: Some("body".into()),
                body_len: 4,
                thread: None,
                thread_seq: 0,
                members: Vec::new(),
                body_path: None,
                body_blake3: None,
                state: dudes_types::MailState::Durable,
                actor: None,
                provenance: None,
            },
        };
        let lsn = writer.append(&entry).unwrap();
        mailbox.apply_entry(lsn, &entry);
    }
    let outgoing = f
        .launch
        .inventory
        .private
        .outbox
        .join(format!("{}.json", dudes_types::MailId::from_parts(1, 5)));
    if matches!(activity, MailboxActivity::Outgoing) {
        std::fs::write(&outgoing, serde_json::to_vec(&serde_json::json!({"to":{"kind":"agent","agent":other},"act":"message","subject":"message","body":"body"})).unwrap()).unwrap();
    }
    mailbox.pass(
        &std::collections::BTreeMap::from([(agent, false), (other, false)]),
        || dudes_types::Timestamp::from_unix_millis(3),
        || dudes_types::ThreadId::from_parts(1, 6),
        |record, at| {
            let entry = Entry {
                at: at.unwrap_or(dudes_types::Timestamp::from_unix_millis(3)),
                record: record.clone(),
            };
            let lsn = writer.append(&entry)?;
            Ok((lsn, entry))
        },
    );
    assert!(
        f.root
            .join("state/agents")
            .join(other.to_string())
            .join("outbox")
            .is_dir()
    );
    if matches!(activity, MailboxActivity::Incoming) {
        assert!(
            f.launch
                .inventory
                .private
                .outbox
                .parent()
                .unwrap()
                .join("inbox")
                .join(format!("{}.json", dudes_types::MailId::from_parts(1, 3)))
                .is_file()
        );
    }
    if matches!(activity, MailboxActivity::Outgoing) {
        assert!(
            !outgoing.exists(),
            "the mailbox consumed the inspected outbox file"
        );
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
    std::fs::write(f.root.join("codex"), b"changed binary").unwrap();
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
                drop(
                    f.profile
                        .mutations
                        .agent(startup.agent_dir())
                        .unwrap()
                        .mutate()
                        .unwrap(),
                );
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
