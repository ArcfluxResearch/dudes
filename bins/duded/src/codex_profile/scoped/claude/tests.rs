use super::super::tests::Fixture;
use super::*;
use dudes_proto::repository::RepositoryEntry;
use dudes_providers::adapter::{Adapter as _, AgentCtx, TurnSession};
use dudes_providers::claude::{ClaudeAdapter, config::ClaudeConfig};
use dudes_providers::scoped::ScopedLaunchInputs;
use dudes_types::{AgentId, Mode, ModelSelection, SessionId};

fn fixture(resume: bool) -> Fixture {
    let mut f = Fixture::new(resume);
    let inventory = &f.launch.inventory;
    let mut private = inventory.private.clone();
    let agent = private.outbox.parent().unwrap().to_owned();
    private.profile = agent.join("profile");
    let git = inventory
        .hired
        .repositories
        .entries()
        .iter()
        .map(|entry| RepositoryEntry {
            repo_id: entry.repo_id.clone(),
            value: entry.value.origin.common_dir.clone(),
        })
        .collect::<Vec<_>>();
    let adapter = ClaudeAdapter::new(ClaudeConfig {
        command: f.launch.spec.argv[0].clone(),
        permission_mode: dudes_providers::claude::config::PermissionMode::Default,
        ..ClaudeConfig::default()
    });
    let session = if resume {
        TurnSession::Resume(SessionId::new("00000000-0000-7000-8000-000000000002"))
    } else {
        TurnSession::First(SessionId::new("00000000-0000-7000-8000-000000000001"))
    };
    f.launch = adapter
        .managed_scoped(
            &AgentCtx {
                agent: AgentId::from_parts(1, 1),
                cwd: &inventory.cwd,
                agent_dir: &agent,
                daemon_env: &[],
                sandbox: None,
                trusted: true,
                max_turns: Some(8),
                max_budget: None,
                mcp_servers: &[],
            },
            &ScopedLaunchInputs {
                grant: &inventory.grant,
                hired: &inventory.hired,
                origin_git_dirs: &git,
                private,
                mode: Mode::Managed,
                platform: if cfg!(target_os = "macos") {
                    ScopedPlatform::MacOsArm64
                } else {
                    ScopedPlatform::LinuxX86_64
                },
            },
            "exact immutable task",
            &session,
            &ModelSelection::named("claude-sonnet-4-6"),
        )
        .unwrap();
    f.profile.private_roots().qualify_claude_fixture(b"binary");
    f
}

#[test]
fn claude_uses_the_existing_worker_and_retains_the_immutable_task_and_install() {
    for resume in [false, true] {
        let f = fixture(resume);
        let (sender, receiver) = std::sync::mpsc::channel();
        let handle = crate::ports::Handle::new(sender);
        let mut workers = crate::chat_launch::LaunchPreparations::new(
            &handle,
            crate::chat_launch::BrowserLaunchPause::default(),
        )
        .unwrap();
        assert!(
            workers.prepare_scoped(crate::chat_launch::ScopedPreparation {
                attempt: Fixture::attempt(),
                launch: f.launch.clone(),
                profile: ScopedProfile::Claude(f.profile.private_roots()),
                outbox: Arc::clone(&f.outbox),
            })
        );
        let crate::ports::Job::ScopedLaunchPrepared(completion) = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
        else {
            panic!("scoped worker completion");
        };
        let prepared = completion.prepared.unwrap();
        assert_eq!(prepared.attempt, Fixture::attempt());
        assert_eq!(prepared.launch.spec, f.launch.spec);
        assert_eq!(prepared.launch.install, f.launch.install);
        assert_eq!(
            prepared.launch.spec.stdin,
            LaunchInput::WriteThenClose(b"exact immutable task".to_vec())
        );
        assert!(
            prepared
                .launch
                .spec
                .argv
                .windows(2)
                .any(|pair| pair[0] == if resume { "--resume" } else { "--session-id" })
        );
        assert!(
            prepared
                .launch
                .install
                .directories
                .iter()
                .all(|root| root.is_dir())
        );
        prepared.binding.before_host(&prepared.launch.spec).unwrap();
        workers.finish_scoped(completion.attempt);
        workers.shutdown();
    }
}

#[test]
fn claude_retained_binding_refuses_changed_task_root_binary_or_owned_mutation() {
    for change in 0..5 {
        let f = fixture(false);
        let roots = f.profile.private_roots();
        let prepared = ScopedProfile::Claude(Arc::clone(&roots))
            .prepare_scoped(f.launch.clone(), Fixture::attempt())
            .unwrap();
        let mut spec = prepared.launch.spec.clone();
        match change {
            0 => spec.stdin = LaunchInput::WriteThenClose(b"different task".to_vec()),
            1 => {
                let root = &prepared.launch.inventory.private.work;
                std::fs::rename(root, f.root.join("retained-work")).unwrap();
                owner_directory(root).unwrap();
            }
            2 => std::fs::write(&spec.argv[0], b"changed").unwrap(),
            3 => {
                drop(
                    roots
                        .agent(prepared.launch.inventory.private.outbox.parent().unwrap())
                        .unwrap()
                        .mutate()
                        .unwrap(),
                );
            }
            _ => {
                std::fs::remove_file(prepared.launch.inventory.cwd.join(".git")).unwrap();
            }
        }
        assert!(
            prepared.binding.before_host(&spec).is_err(),
            "change {change}"
        );
    }
}

#[test]
fn claude_preparation_refuses_widened_installs_or_private_environment() {
    for change in 0..4 {
        let mut f = fixture(false);
        match change {
            0 => f.launch.install.directories.push(f.root.join("extra")),
            1 => f.launch.install.directories.clear(),
            2 => f
                .launch
                .spec
                .env
                .push(EnvVar::new("NODE_OPTIONS", "unowned")),
            _ => f.launch.spec.env.push(EnvVar::new("TMPDIR", &f.root)),
        }
        assert!(
            ScopedProfile::Claude(f.profile.private_roots())
                .prepare_scoped(f.launch, Fixture::attempt())
                .is_err()
        );
    }
}

#[test]
fn claude_platform_and_linux_socket_paths_are_bounded_without_relocation() {
    assert!(qualification(ScopedPlatform::Unsupported, Path::new("/tmp/a")).is_err());
    assert!(linux_temp(Path::new(&format!("/{}", "x".repeat(71)))).is_ok());
    assert!(linux_temp(Path::new(&format!("/{}", "x".repeat(72)))).is_err());
}

#[test]
fn claude_preparation_survives_inbox_publication_and_unrelated_mailbox_setup() {
    use super::super::tests::{MailboxActivity, mailbox_during_startup};
    for activity in [
        MailboxActivity::NoMail,
        MailboxActivity::Incoming,
        MailboxActivity::Outgoing,
    ] {
        let f = fixture(true);
        let prepared = ScopedProfile::Claude(f.profile.private_roots())
            .prepare_scoped(f.launch.clone(), Fixture::attempt())
            .unwrap();
        mailbox_during_startup(&f, activity);
        assert_eq!(
            prepared.binding.before_host(&prepared.launch.spec).is_ok(),
            !matches!(activity, MailboxActivity::Outgoing)
        );
    }
}
