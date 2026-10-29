//! Pure candidate composition and legacy compatibility checks.
#![cfg(unix)]

use std::path::{Path, PathBuf};

use dudes_proto::record::{FileIdentity, RecordedWorkspace, TrustedRepository};
use dudes_proto::repository::{
    ExecutionRepository, OwnedRepository, RepoId, RepositoryEntry, RepositoryPolicy, RepositorySet,
    RepositoryTurnGrant, ScopedHireRepositories, ScopedRepositories,
};
use dudes_providers::adapter::{Adapter, AgentCtx, TurnSession};
use dudes_providers::claude::{
    ClaudeAdapter,
    config::{ClaudeConfig, PermissionMode},
};
use dudes_providers::codex::{CodexAdapter, config::CodexConfig};
use dudes_providers::launch::{EnvVar, LaunchError, LaunchInput};
use dudes_providers::mcp::{HttpAuth, HttpServer, McpLaunch, McpServer};
use dudes_providers::pi::{PiAdapter, config::PiConfig};
use dudes_providers::protocol::ProtocolAction;
use dudes_providers::scoped::{
    ScopedError, ScopedLaunchInputs, ScopedModelProfile, ScopedPlatform, ScopedPrivatePaths,
    ScopedProofGap,
};
use dudes_types::{
    AgentId, GitCommit, IsolationMode, Lsn, McpServerName, Mode, ModelSelection, SessionId,
    WorktreeId,
};
use serde_json::{Value, json};

const SESSION: &str = "550e8400-e29b-41d4-a716-446655440000";
const THREAD: &str = "00000000-0000-7000-8000-000000000002";

struct Fixture {
    hired: ScopedRepositories,
    grant: RepositoryTurnGrant,
    git: Vec<RepositoryEntry<PathBuf>>,
    private: ScopedPrivatePaths,
}

impl Fixture {
    fn new(order: &[&str]) -> Self {
        let entries: Vec<_> = ["a", "b", "c"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                let identity = FileIdentity {
                    device: 1,
                    inode: (index as u64 + 1) * 10,
                };
                RepositoryEntry {
                    repo_id: RepoId::parse(name).unwrap(),
                    value: OwnedRepository {
                        origin: TrustedRepository {
                            directory: format!("/origins/{name}").into(),
                            directory_identity: identity,
                            common_dir: format!("/git/{name}").into(),
                            common_identity: identity,
                        },
                        execution: ExecutionRepository {
                            directory: format!("/copies/{name}").into(),
                            directory_identity: identity,
                            git_dir: format!("/git/{name}/agent").into(),
                            git_identity: identity,
                        },
                        worktree: WorktreeId::from_bytes([u8::try_from(index).unwrap(); 16]),
                        base: GitCommit::parse(&"a".repeat(40)).unwrap(),
                        trust_lsn: Lsn::from_parts(0, 10).unwrap(),
                    },
                }
            })
            .collect();
        let primary = RepoId::parse("a").unwrap();
        let hired = ScopedRepositories {
            project: RecordedWorkspace {
                id: "project".into(),
                label: "Project".into(),
            },
            catalog_lsn: Lsn::from_parts(0, 20).unwrap(),
            policy: RepositoryPolicy::WorkingTreeV1,
            repositories: RepositorySet::new(primary, entries).unwrap(),
        };
        let selected = order
            .iter()
            .map(|name| {
                let repo_id = RepoId::parse(name).unwrap();
                RepositoryEntry {
                    value: hired.repositories.get(&repo_id).unwrap().clone(),
                    repo_id,
                }
            })
            .collect();
        let grant = RepositoryTurnGrant {
            hired_lsn: Lsn::from_parts(0, 30).unwrap(),
            project: hired.project.id.clone(),
            catalog_lsn: hired.catalog_lsn,
            policy: hired.policy,
            repositories: RepositorySet::new(RepoId::parse(order[0]).unwrap(), selected).unwrap(),
            ticket: None,
        };
        let git = hired
            .repositories
            .entries()
            .iter()
            .map(|entry| RepositoryEntry {
                repo_id: entry.repo_id.clone(),
                value: entry.value.origin.common_dir.join("origin"),
            })
            .collect();
        let private = ScopedPrivatePaths {
            home: "/private/home".into(),
            profile: "/private/profile".into(),
            work: "/private/agent/work".into(),
            outbox: "/private/agent/outbox".into(),
            temp: "/private/agent/tmp".into(),
        };
        Self {
            hired,
            grant,
            git,
            private,
        }
    }

    fn scope(&self) -> ScopedLaunchInputs<'_> {
        ScopedLaunchInputs {
            grant: &self.grant,
            hired: &self.hired,
            origin_git_dirs: &self.git,
            private: self.private.clone(),
            mode: Mode::Managed,
            platform: ScopedPlatform::MacOsArm64,
        }
    }

    fn ctx<'a>(&'a self, env: &'a [EnvVar]) -> AgentCtx<'a> {
        AgentCtx {
            agent: AgentId::from_parts(1, 1),
            cwd: &self.hired.repositories.primary().execution.directory,
            agent_dir: self.private.outbox.parent().unwrap(),
            daemon_env: env,
            sandbox: Some(true),
            trusted: true,
            max_turns: Some(8),
            max_budget: None,
            mcp_servers: &[],
        }
    }
}

fn claude() -> ClaudeAdapter {
    ClaudeAdapter::new(ClaudeConfig {
        permission_mode: PermissionMode::Default,
        ..ClaudeConfig::default()
    })
}

fn codex() -> CodexAdapter {
    CodexAdapter::new(CodexConfig {
        streaming: true,
        ..CodexConfig::default()
    })
    .with_managed_home("/private/profile".into())
}

fn first() -> TurnSession {
    TurnSession::First(SessionId::new(SESSION))
}

fn setting(spec: &dudes_providers::launch::LaunchSpec) -> Value {
    serde_json::from_str(
        &spec
            .argv
            .windows(2)
            .find(|words| words[0] == "--settings")
            .unwrap()[1]
            .to_string_lossy(),
    )
    .unwrap()
}

#[test]
fn declaration_order_and_all_hire_metadata_share_one_unarmed_plan() {
    let f = Fixture::new(&["b", "a"]);
    let recipe = claude()
        .managed_scoped(
            &f.ctx(&[]),
            &f.scope(),
            "task",
            &first(),
            &ModelSelection::named("claude-sonnet-4-6"),
        )
        .unwrap();
    assert_eq!(recipe.spec.cwd, Path::new("/copies/b"));
    assert_eq!(
        recipe.inventory.writable_roots(),
        [
            "/copies/b",
            "/copies/a",
            "/private/agent/work",
            "/private/agent/outbox",
            "/private/agent/tmp"
        ]
        .map(PathBuf::from)
    );
    assert_eq!(recipe.inventory.grant, f.grant);
    assert_eq!(recipe.inventory.hired, f.hired);
    for path in [
        "/origins/c/.git",
        "/copies/c/.git",
        "/git/c/agent",
        "/git/c/origin",
        "/git/c",
        "/private/home",
        "/private/profile",
        "/private/agent/inbox",
    ] {
        assert!(
            recipe.inventory.protected.contains(&PathBuf::from(path)),
            "missing {path}"
        );
    }
    let settings = setting(&recipe.spec);
    assert_eq!(
        settings["sandbox"]["filesystem"]["allowWrite"],
        json!(recipe.inventory.writable_roots())
    );
    assert_eq!(
        settings["sandbox"]["filesystem"]["denyWrite"],
        json!(recipe.inventory.protected)
    );
    assert_eq!(
        settings["sandbox"]["network"]["allowUnixSockets"],
        json!([])
    );
    assert_eq!(settings["sandbox"]["failIfUnavailable"], true);
    assert_eq!(
        recipe.spec.stdin,
        LaunchInput::WriteThenClose(b"task".to_vec())
    );
    assert!(
        !recipe
            .spec
            .argv
            .iter()
            .any(|arg| arg == "--no-session-persistence")
    );
    assert!(
        recipe
            .spec
            .argv
            .windows(2)
            .any(|args| args[0] == "--session-id" && args[1] == SESSION)
    );
    assert!(
        recipe
            .unarmed
            .missing_proofs
            .contains(&ScopedProofGap::ClaudePersistence)
    );
}

#[test]
fn general_grant_uses_its_recorded_primary_without_reordering_roots() {
    let mut f = Fixture::new(&["a", "b", "c"]);
    f.hired.repositories = RepositorySet::new(
        RepoId::parse("b").unwrap(),
        f.hired.repositories.entries().to_vec(),
    )
    .unwrap();
    let mut normalized = serde_json::to_value(&f.hired).unwrap();
    for entry in normalized["repositories"]["entries"]
        .as_array_mut()
        .unwrap()
    {
        entry["value"] = json!({
            "recorded": entry["value"].take(),
            "original_pin": {"attributes": "0".repeat(64), "config": "0".repeat(64)},
        });
    }
    let hire: ScopedHireRepositories = serde_json::from_value(normalized).unwrap();
    f.grant = RepositoryTurnGrant::select(Lsn::from_parts(0, 30).unwrap(), &hire, None).unwrap();
    assert_eq!(f.grant.repositories, f.hired.repositories);
    for recipe in [
        claude()
            .managed_scoped(
                &f.ctx(&[]),
                &f.scope(),
                "task",
                &first(),
                &ModelSelection::named("claude-sonnet-4-6"),
            )
            .unwrap(),
        codex()
            .managed_scoped(
                &f.ctx(&[]),
                &f.scope(),
                "task",
                &TurnSession::Assigned,
                &ModelSelection::named("gpt-5.5"),
            )
            .unwrap(),
    ] {
        assert_eq!(recipe.spec.cwd, Path::new("/copies/b"));
        assert_eq!(recipe.inventory.cwd, recipe.spec.cwd);
        assert_eq!(
            recipe.inventory.writable_roots(),
            [
                "/copies/a",
                "/copies/b",
                "/copies/c",
                "/private/agent/work",
                "/private/agent/outbox",
                "/private/agent/tmp",
            ]
            .map(PathBuf::from)
        );
        assert_eq!(
            recipe.inventory.additional_roots(),
            [
                "/copies/a",
                "/copies/c",
                "/private/agent/work",
                "/private/agent/outbox",
                "/private/agent/tmp",
            ]
            .map(PathBuf::from)
        );
        if let LaunchInput::Protocol(startup) = &recipe.spec.stdin {
            assert_eq!(startup.cwd(), Path::new("/copies/b"));
        }
        for repo in ["a", "b", "c"] {
            for path in [
                format!("/origins/{repo}/.git"),
                format!("/copies/{repo}/.git"),
                format!("/git/{repo}/agent"),
                format!("/git/{repo}/origin"),
                format!("/git/{repo}"),
            ] {
                assert!(recipe.inventory.protected.contains(&PathBuf::from(path)));
            }
        }
    }
}

#[test]
fn excluded_hire_primary_is_not_regained_by_cwd_or_additional_roots() {
    let f = Fixture::new(&["b"]);
    for recipe in [
        claude()
            .managed_scoped(
                &f.ctx(&[]),
                &f.scope(),
                "task",
                &first(),
                &ModelSelection::named("claude-sonnet-4-6"),
            )
            .unwrap(),
        codex()
            .managed_scoped(
                &f.ctx(&[]),
                &f.scope(),
                "task",
                &TurnSession::Assigned,
                &ModelSelection::named("gpt-5.5"),
            )
            .unwrap(),
    ] {
        assert_eq!(recipe.spec.cwd, Path::new("/copies/b"));
        assert!(
            !recipe
                .inventory
                .writable_roots()
                .iter()
                .any(|root| root == Path::new("/copies/a") || root == Path::new("/copies/c"))
        );
        assert!(
            !recipe
                .inventory
                .writable_roots()
                .contains(&PathBuf::from("/private/agent"))
        );
    }
}

#[test]
fn claude_resume_retains_the_exact_session_and_only_the_current_grant() {
    let mut f = Fixture::new(&["b", "a"]);
    let adapter = claude();
    let model = ModelSelection::named("claude-sonnet-4-6");
    let previous = adapter
        .managed_scoped(&f.ctx(&[]), &f.scope(), "previous task", &first(), &model)
        .unwrap();
    assert!(
        previous
            .inventory
            .writable_roots()
            .contains(&PathBuf::from("/copies/a"))
    );
    f.grant.repositories = RepositorySet::new(
        RepoId::parse("b").unwrap(),
        vec![f.grant.repositories.entries()[0].clone()],
    )
    .unwrap();
    let task = "the immutable resumed task";
    let resumed = adapter
        .managed_scoped(
            &f.ctx(&[]),
            &f.scope(),
            task,
            &TurnSession::Resume(SessionId::new(SESSION)),
            &model,
        )
        .unwrap();
    assert_eq!(resumed.inventory.grant, f.grant);
    assert_eq!(resumed.spec.cwd, Path::new("/copies/b"));
    assert_eq!(
        setting(&resumed.spec)["sandbox"]["filesystem"]["allowWrite"],
        json!([
            "/copies/b",
            "/private/agent/work",
            "/private/agent/outbox",
            "/private/agent/tmp"
        ])
    );
    assert_eq!(
        resumed.spec.stdin,
        LaunchInput::WriteThenClose(task.as_bytes().to_vec())
    );
    let mut fresh = adapter
        .managed_scoped(&f.ctx(&[]), &f.scope(), task, &first(), &model)
        .unwrap();
    let session_flag = fresh
        .spec
        .argv
        .iter()
        .position(|arg| arg == "--session-id")
        .unwrap();
    assert_eq!(fresh.spec.argv[session_flag + 1], SESSION);
    fresh.spec.argv[session_flag] = "--resume".into();
    assert_eq!(resumed, fresh);
    assert!(
        resumed
            .unarmed
            .missing_proofs
            .contains(&ScopedProofGap::RuntimeAdmission)
    );
}

fn open_response(recipe: &dudes_providers::scoped::ScopedManagedLaunch, extra: bool) -> Value {
    let mut union = vec![recipe.spec.cwd.clone()];
    union.extend(recipe.inventory.additional_roots());
    if extra {
        union.push("/copies/c".into());
    }
    json!({"id":2,"result":{"thread":{"id":THREAD,"status":{"type":"idle"}},"cwd":recipe.spec.cwd,"model":"gpt-5.5","approvalPolicy":"never","sandbox":{"type":"workspaceWrite","writableRoots":recipe.inventory.additional_roots(),"networkAccess":false,"excludeTmpdirEnvVar":true,"excludeSlashTmp":true},"runtimeWorkspaceRoots":union}})
}

#[test]
fn codex_profile_startup_and_task_have_the_exact_explicit_and_implicit_union() {
    let f = Fixture::new(&["b", "a"]);
    let adapter = codex();
    let recipe = adapter
        .managed_scoped(
            &f.ctx(&[]),
            &f.scope(),
            "task",
            &TurnSession::Assigned,
            &ModelSelection::named("gpt-5.5"),
        )
        .unwrap();
    let roots = recipe
        .spec
        .argv
        .iter()
        .find_map(|arg| {
            arg.to_str()?
                .strip_prefix("sandbox_workspace_write.writable_roots=")
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(roots).unwrap(),
        json!(recipe.inventory.additional_roots())
    );
    assert_eq!(recipe.unarmed.model_profile, ScopedModelProfile::CodexGpt55);
    assert!(
        recipe
            .unarmed
            .missing_proofs
            .contains(&ScopedProofGap::CodexModelTools)
    );
    for mutation in ["matching", "extra_root", "root_order"] {
        let mut reader = adapter.turn_reader_for(&recipe.spec);
        reader.start();
        let opened=reader.line(&json!({"id":1,"result":{"userAgent":"dudes/0.164.0-alpha.11.1 (fixture)","codexHome":"/private/profile"}}).to_string());
        assert_eq!(opened.actions.len(), 2);
        let mut response = open_response(&recipe, mutation == "extra_root");
        if mutation == "root_order" {
            response["result"]["runtimeWorkspaceRoots"]
                .as_array_mut()
                .unwrap()
                .swap(0, 1);
        }
        let read = reader.line(&response.to_string());
        if mutation == "matching" {
            let ProtocolAction::Write(frame) = &read.actions[0] else {
                panic!("task frame");
            };
            assert!(frame.is_task());
            let task: Value = serde_json::from_slice(frame.bytes()).unwrap();
            assert_eq!(
                task["params"]["sandboxPolicy"]["writableRoots"],
                json!(recipe.inventory.additional_roots())
            );
            assert_eq!(task["params"]["cwd"], "/copies/b");
            assert_eq!(task["params"]["input"][0]["text"], "task");
        } else {
            assert_eq!(read.actions, vec![ProtocolAction::Fail]);
        }
    }
}

#[test]
fn codex_resume_checks_the_current_posture_before_submitting_one_task() {
    let mut f = Fixture::new(&["b", "a"]);
    let adapter = codex();
    let model = ModelSelection::named("gpt-5.5");
    let previous = adapter
        .managed_scoped(
            &f.ctx(&[]),
            &f.scope(),
            "previous task",
            &TurnSession::Assigned,
            &model,
        )
        .unwrap();
    f.grant.repositories = RepositorySet::new(
        RepoId::parse("b").unwrap(),
        vec![f.grant.repositories.entries()[0].clone()],
    )
    .unwrap();
    let recipe = adapter
        .managed_scoped(
            &f.ctx(&[]),
            &f.scope(),
            "the immutable resumed task",
            &TurnSession::Resume(SessionId::new(THREAD)),
            &model,
        )
        .unwrap();
    assert_eq!(recipe.inventory.grant, f.grant);
    assert!(
        recipe
            .unarmed
            .missing_proofs
            .contains(&ScopedProofGap::RuntimeAdmission)
    );
    for posture in [
        "matching",
        "previous_roots",
        "wrong_thread",
        "wrong_cwd",
        "wrong_model",
        "busy",
    ] {
        let mut reader = adapter.turn_reader_for(&recipe.spec);
        let initial = reader.start();
        assert!(matches!(initial.as_slice(), [ProtocolAction::Write(frame)] if !frame.is_task()));
        let opened = reader.line(
            &json!({"id":1,"result":{"userAgent":"dudes/0.164.0-alpha.11.1 (fixture)","codexHome":"/private/profile"}}).to_string(),
        );
        let [
            ProtocolAction::Write(initialized),
            ProtocolAction::Write(frame),
        ] = opened.actions.as_slice()
        else {
            panic!("initialization and resume request");
        };
        assert!(!initialized.is_task() && !frame.is_task());
        let request: Value = serde_json::from_slice(frame.bytes()).unwrap();
        assert_eq!(request["method"], "thread/resume");
        assert_eq!(request["params"]["threadId"], THREAD);
        assert_eq!(request["params"]["excludeTurns"], true);
        let mut response = open_response(&recipe, false);
        match posture {
            "previous_roots" => response = open_response(&previous, false),
            "wrong_thread" => response["result"]["thread"]["id"] = SESSION.into(),
            "wrong_cwd" => response["result"]["cwd"] = "/copies/a".into(),
            "wrong_model" => response["result"]["model"] = "gpt-6.1".into(),
            "busy" => response["result"]["thread"]["status"]["type"] = "active".into(),
            "matching" => {}
            _ => unreachable!(),
        }
        let read = reader.line(&response.to_string());
        if posture == "matching" {
            let [ProtocolAction::Write(frame)] = read.actions.as_slice() else {
                panic!("one immutable task");
            };
            assert!(frame.is_task());
            let task: Value = serde_json::from_slice(frame.bytes()).unwrap();
            assert_eq!(task["method"], "turn/start");
            assert_eq!(task["params"]["threadId"], THREAD);
            assert_eq!(task["params"]["cwd"], "/copies/b");
            assert_eq!(task["params"]["model"], "gpt-5.5");
            assert_eq!(
                task["params"]["input"],
                json!([{"type":"text","text":"the immutable resumed task","text_elements":[]}])
            );
            assert_eq!(
                task["params"]["sandboxPolicy"]["writableRoots"],
                json!([
                    "/private/agent/work",
                    "/private/agent/outbox",
                    "/private/agent/tmp"
                ])
            );
        } else {
            assert_eq!(read.actions, [ProtocolAction::Fail]);
            assert!(read.session.is_none());
        }
        assert!(reader.line(&response.to_string()).actions.is_empty());
        assert_eq!(reader.start(), [ProtocolAction::Fail]);
    }
}

#[test]
fn incompatible_sessions_models_modes_and_platforms_refuse_without_legacy_fallback() {
    let f = Fixture::new(&["b"]);
    let ctx = f.ctx(&[]);
    assert_eq!(
        claude()
            .managed_scoped(
                &ctx,
                &f.scope(),
                "task",
                &TurnSession::Assigned,
                &ModelSelection::named("claude-sonnet-4-6")
            )
            .unwrap_err(),
        LaunchError::Scoped(ScopedError::Session)
    );
    assert_eq!(
        codex()
            .managed_scoped(
                &ctx,
                &f.scope(),
                "task",
                &first(),
                &ModelSelection::named("gpt-5.5")
            )
            .unwrap_err(),
        LaunchError::Scoped(ScopedError::Session)
    );
    for model in [
        ModelSelection::provider_default(),
        ModelSelection::named("gpt-6.1"),
        ModelSelection::named("gpt-6.1-sol"),
    ] {
        assert_eq!(
            codex()
                .managed_scoped(&ctx, &f.scope(), "task", &TurnSession::Assigned, &model)
                .unwrap_err(),
            LaunchError::Scoped(ScopedError::Model)
        );
    }
    let mut scope = f.scope();
    scope.mode = Mode::Attached;
    assert_eq!(
        claude()
            .managed_scoped(
                &ctx,
                &scope,
                "task",
                &first(),
                &ModelSelection::named("claude-sonnet-4-6")
            )
            .unwrap_err(),
        LaunchError::Scoped(ScopedError::ModeOrPlatform)
    );
    scope.mode = Mode::Managed;
    scope.platform = ScopedPlatform::Unsupported;
    assert_eq!(
        codex()
            .managed_scoped(
                &ctx,
                &scope,
                "task",
                &TurnSession::Assigned,
                &ModelSelection::named("gpt-5.5")
            )
            .unwrap_err(),
        LaunchError::Scoped(ScopedError::ModeOrPlatform)
    );
    let pi = PiAdapter::new(PiConfig::default());
    assert_eq!(
        pi.managed_scoped(
            &ctx,
            &f.scope(),
            "task",
            &TurnSession::Assigned,
            &ModelSelection::provider_default()
        )
        .unwrap_err(),
        LaunchError::Scoped(ScopedError::Provider)
    );
}

#[test]
fn incompatible_options_and_authority_are_refused() {
    let f = Fixture::new(&["b"]);
    let ctx = f.ctx(&[]);
    let model = ModelSelection::named("claude-sonnet-4-6");
    assert_eq!(
        CodexAdapter::default()
            .managed_scoped(
                &ctx,
                &f.scope(),
                "task",
                &TurnSession::Assigned,
                &ModelSelection::named("gpt-5.5")
            )
            .unwrap_err(),
        LaunchError::Scoped(ScopedError::Options)
    );
    for config in [
        ClaudeConfig {
            permission_mode: PermissionMode::Default,
            extra_args: vec!["--add-dir".into(), "/copies/c".into()],
            ..ClaudeConfig::default()
        },
        ClaudeConfig {
            permission_mode: PermissionMode::Default,
            isolation: IsolationMode::Inherit,
            ..ClaudeConfig::default()
        },
    ] {
        assert_eq!(
            ClaudeAdapter::new(config)
                .managed_scoped(&ctx, &f.scope(), "task", &first(), &model)
                .unwrap_err(),
            LaunchError::Scoped(ScopedError::Options)
        );
    }
    let mut disabled = f.ctx(&[]);
    disabled.sandbox = Some(false);
    assert_eq!(
        claude()
            .managed_scoped(&disabled, &f.scope(), "task", &first(), &model)
            .unwrap_err(),
        LaunchError::Scoped(ScopedError::Options)
    );
    let servers = [McpLaunch {
        name: McpServerName::parse("server").unwrap(),
        server: McpServer::Http(HttpServer {
            url: "http://127.0.0.1:1/mcp".into(),
            auth: HttpAuth::None,
        }),
        env: vec![],
    }];
    let mut opted = f.ctx(&[]);
    opted.mcp_servers = &servers;
    assert_eq!(
        claude()
            .managed_scoped(&opted, &f.scope(), "task", &first(), &model)
            .unwrap_err(),
        LaunchError::Scoped(ScopedError::Options)
    );
    let mut scope = f.scope();
    scope.private.outbox = "/other/agent/outbox".into();
    assert_eq!(
        claude()
            .managed_scoped(&ctx, &scope, "task", &first(), &model)
            .unwrap_err(),
        LaunchError::Scoped(ScopedError::Paths)
    );
    let mut bad = Fixture::new(&["b"]);
    bad.grant.catalog_lsn = Lsn::from_parts(0, 21).unwrap();
    assert_eq!(
        claude()
            .managed_scoped(&ctx, &bad.scope(), "task", &first(), &model)
            .unwrap_err(),
        LaunchError::Scoped(ScopedError::Authority)
    );
}

#[test]
fn private_control_locations_and_literal_path_patterns_cannot_expand_authority() {
    let f = Fixture::new(&["b"]);
    let ctx = f.ctx(&[]);
    let model = ModelSelection::named("claude-sonnet-4-6");
    let mut scope = f.scope();
    scope.private.home = "/copies/c/settings".into();
    assert_eq!(
        claude()
            .managed_scoped(&ctx, &scope, "task", &first(), &model)
            .unwrap_err(),
        LaunchError::Scoped(ScopedError::Paths)
    );
    for spelling in [
        "/copies/b[1]",
        "/copies/b)",
        "/copies/b\nother",
        "/copies/b\"quoted",
        "/copies/b ",
    ] {
        let mut bad = Fixture::new(&["b"]);
        let mut entries = bad.hired.repositories.entries().to_vec();
        entries[1].value.execution.directory = spelling.into();
        let copy = entries[1].clone();
        bad.hired.repositories = RepositorySet::new(RepoId::parse("a").unwrap(), entries).unwrap();
        bad.grant.repositories =
            RepositorySet::new(RepoId::parse("b").unwrap(), vec![copy]).unwrap();
        assert_eq!(
            claude()
                .managed_scoped(&ctx, &bad.scope(), "task", &first(), &model)
                .unwrap_err(),
            LaunchError::Scoped(ScopedError::Paths)
        );
    }
}

#[test]
fn inbox_cannot_be_selected_as_a_checkout() {
    let mut f = Fixture::new(&["b"]);
    let mut entries = f.hired.repositories.entries().to_vec();
    entries[1].value.execution.directory = "/private/agent/inbox".into();
    let selected = entries[1].clone();
    f.hired.repositories = RepositorySet::new(RepoId::parse("a").unwrap(), entries).unwrap();
    f.grant.repositories = RepositorySet::new(RepoId::parse("b").unwrap(), vec![selected]).unwrap();
    let ctx = f.ctx(&[]);
    let results = [
        claude()
            .managed_scoped(
                &ctx,
                &f.scope(),
                "task",
                &first(),
                &ModelSelection::named("claude-sonnet-4-6"),
            )
            .map(|candidate| candidate.inventory.writable_roots()),
        codex()
            .managed_scoped(
                &ctx,
                &f.scope(),
                "task",
                &TurnSession::Assigned,
                &ModelSelection::named("gpt-5.5"),
            )
            .map(|candidate| candidate.inventory.writable_roots()),
    ];
    assert_eq!(
        results,
        [
            Err(LaunchError::Scoped(ScopedError::Paths)),
            Err(LaunchError::Scoped(ScopedError::Paths))
        ]
    );
}

#[test]
fn environment_is_selected_and_legacy_launch_and_install_bytes_stay_unchanged() {
    let f = Fixture::new(&["b"]);
    let env = [
        EnvVar::new("PATH", "/synthetic/bin"),
        EnvVar::new("ANTHROPIC_API_KEY", "synthetic-key"),
        EnvVar::new("BASH_ENV", "/untrusted/startup"),
        EnvVar::new("CLAUDE_ENV_FILE", "/untrusted/guard"),
        EnvVar::new("HOME", "/untrusted/home"),
    ];
    let ctx = f.ctx(&env);
    let adapter = claude();
    let model = ModelSelection::named("claude-sonnet-4-6");
    let before = adapter.managed(&ctx, "task", &first(), &model).unwrap();
    let install = adapter.managed_install(&ctx, &model);
    let scoped = adapter
        .managed_scoped(&ctx, &f.scope(), "task", &first(), &model)
        .unwrap();
    assert_eq!(
        before,
        adapter.managed(&ctx, "task", &first(), &model).unwrap()
    );
    assert_eq!(install, adapter.managed_install(&ctx, &model));
    assert!(
        !scoped
            .spec
            .env
            .iter()
            .any(|var| var.name() == "BASH_ENV" || var.name() == "CLAUDE_ENV_FILE")
    );
    assert!(
        scoped
            .spec
            .env
            .contains(&EnvVar::new("HOME", "/private/home"))
    );
    let adapter = codex();
    let ctx = f.ctx(&[]);
    let model = ModelSelection::named("gpt-5.5");
    let before = adapter
        .managed(&ctx, "task", &TurnSession::Assigned, &model)
        .unwrap();
    let install = adapter.managed_install(&ctx, &model);
    let listing = dudes_providers::codex::model_list::argv("codex");
    assert!(before.argv.iter().chain(&listing).all(|arg| {
        !arg.to_str()
            .is_some_and(|word| word.starts_with("features.goals="))
    }));
    let scoped = adapter
        .managed_scoped(&ctx, &f.scope(), "task", &TurnSession::Assigned, &model)
        .unwrap();
    assert_eq!(
        scoped
            .spec
            .argv
            .windows(2)
            .filter(|args| args[0] == "-c" && args[1] == "features.goals=false")
            .count(),
        1
    );
    assert_eq!(
        before,
        adapter
            .managed(&ctx, "task", &TurnSession::Assigned, &model)
            .unwrap()
    );
    assert_eq!(install, adapter.managed_install(&ctx, &model));
}

#[test]
fn file_mail_transport_comes_from_the_owned_agent_context() {
    let f = Fixture::new(&["b"]);
    let ambient = [
        EnvVar::new("PATH", "/synthetic/bin"),
        EnvVar::new("LANG", "C"),
        EnvVar::new("ANTHROPIC_API_KEY", "synthetic-anthropic-key"),
        EnvVar::new("ANTHROPIC_BASE_URL", "https://synthetic.invalid"),
        EnvVar::new("OPENAI_API_KEY", "synthetic-openai-key"),
        EnvVar::new("CODEX_API_KEY", "synthetic-codex-key"),
        EnvVar::new("DUDES_MAILBOX", "/untrusted/mailbox"),
        EnvVar::new("DUDES_COMMAND_TRANSPORT", "socket"),
        EnvVar::new("DUDES_TOKEN", "synthetic-token"),
        EnvVar::new("DUDES_SOCK", "/untrusted/socket"),
        EnvVar::new("DUDES_RUNTIME_DIR", "/untrusted/runtime"),
        EnvVar::new("DUDES_STATE_DIR", "/untrusted/state"),
        EnvVar::new("DUDES_CONFIG_DIR", "/untrusted/config"),
        EnvVar::new("DUDES_CONFIG", "/untrusted/operator.toml"),
    ];
    let ctx = f.ctx(&ambient);
    let claude = claude()
        .managed_scoped(
            &ctx,
            &f.scope(),
            "task",
            &first(),
            &ModelSelection::named("claude-sonnet-4-6"),
        )
        .unwrap();
    let codex = codex()
        .managed_scoped(
            &ctx,
            &f.scope(),
            "task",
            &TurnSession::Assigned,
            &ModelSelection::named("gpt-5.5"),
        )
        .unwrap();
    for recipe in [&claude, &codex] {
        let env = &recipe.spec.env;
        assert!(env.contains(&EnvVar::new("DUDES_MAILBOX", ctx.agent_dir)));
        assert!(env.contains(&EnvVar::new("DUDES_COMMAND_TRANSPORT", "file-mail")));
        assert_eq!(
            env.iter()
                .filter(|var| var.name().to_string_lossy().starts_with("DUDES_"))
                .count(),
            2
        );
        assert!(env.contains(&ambient[0]));
        assert!(env.contains(&ambient[1]));
    }
    assert!(claude.spec.env.contains(&ambient[2]));
    assert!(claude.spec.env.contains(&ambient[3]));
    assert!(!claude.spec.env.contains(&ambient[4]));
    assert!(!claude.spec.env.contains(&ambient[5]));
    assert!(codex.spec.env.contains(&ambient[4]));
    assert!(codex.spec.env.contains(&ambient[5]));
    assert!(!codex.spec.env.contains(&ambient[2]));
    assert!(!codex.spec.env.contains(&ambient[3]));
}
