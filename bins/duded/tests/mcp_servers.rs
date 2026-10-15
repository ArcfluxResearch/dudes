//! Operator-declared MCP servers through the daemon: the boot split of the declared variables, the
//! hire's admission in its order, and what an opted-in launch carries.
#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::ExitCause;
use duded::settings::RunOptions;
use duded::startup::Witness;
use dudes_proto::Record;
use dudes_proto::rpc::agent::HireParams;
use dudes_proto::rpc::{ErrorCode, RpcError};
use dudes_providers::adapter::Adapter;
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::codex::CodexAdapter;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_providers::pi::PiAdapter;
use dudes_providers::registry::Adapters;
use dudes_types::{AgentId, IsolationMode, McpServerName, Provider};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

const MEMORY_AUTH: &str = "planted-memory-header";
const NOTES_API_KEY: &str = "planted-notes-secret";

/// `memory` and `search` are http servers, `notes` a program server; `search` names a variable
/// the daemon's environment never holds.
const DECLARED: &str = r#"
[memory]
url = "https://memory.example.com/mcp"
header_env = { "Authorization" = "MEMORY_AUTH" }

[notes]
command = "/usr/bin/true"
secret_env = ["NOTES_API_KEY"]

[folder]
command = "/usr/bin/true"
args = ["--root", "{agent_dir}"]

[search]
url = "http://127.0.0.1:8931/mcp"
bearer_env = "SEARCH_KEY"
"#;

fn options() -> RunOptions {
    RunOptions::default().with_mcp_servers(
        dudes_providers::mcp::parse_servers(toml::from_str(DECLARED).expect("a TOML table"))
            .expect("a valid declaration"),
    )
}

fn names(names: &[&str]) -> Vec<McpServerName> {
    names
        .iter()
        .map(|name| McpServerName::parse(name).expect("a server name"))
        .collect()
}

struct Daemon {
    fixture: Fixture,
    host: SharedHost,
    predicates: Arc<ScriptedRunner>,
    running: common::Running,
    append_failure: duded::runtime::AppendFailure,
}

/// A daemon over Claude, Codex and Pi whose environment holds the two planted secrets and an
/// unrelated variable.
fn boot() -> Daemon {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.daemon_env.extend([
        EnvVar::new("MEMORY_AUTH", MEMORY_AUTH),
        EnvVar::new("NOTES_API_KEY", NOTES_API_KEY),
        EnvVar::new("UNRELATED", "kept"),
    ]);
    let host = SharedHost::new();
    let predicates = Arc::new(ScriptedRunner::default());
    let mut seams = seams(
        &settings,
        host.clone(),
        Arc::clone(&predicates),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    seams.adapters = Adapters::new([
        Box::new(ClaudeAdapter::default()) as Box<dyn Adapter>,
        Box::new(CodexAdapter::default()),
        Box::new(PiAdapter::default()),
    ]);
    let daemon = duded::boot::boot_with_options(&settings, seams, options()).expect("boot");
    let append_failure = daemon.append_failure();
    Daemon {
        fixture,
        host,
        predicates,
        running: common::start(daemon),
        append_failure,
    }
}

fn hire(daemon: &Daemon, change: impl FnOnce(&mut HireParams)) -> Result<AgentId, RpcError> {
    let mut params = common::hire_params(daemon.fixture.state.path());
    change(&mut params);
    match daemon
        .running
        .handle
        .ask(duded::ports::Ask::Hire(Box::new(params)))?
    {
        duded::ports::Answered::Hired(hired) => Ok(hired.agent),
        other => panic!("agent.hire answered {other:?}"),
    }
}

fn hired_count(fixture: &Fixture) -> usize {
    fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::AgentHired { .. }))
        .count()
}

/// One hire the daemon refuses: what it asks for, and the code and message it gets.
struct Refused {
    provider: Provider,
    isolation: Option<IsolationMode>,
    servers: &'static [&'static str],
    code: ErrorCode,
    message: &'static str,
}

/// Each hire rule, in the order the daemon applies them, refuses with its code and message and
/// journals nothing; where two rules both apply, the earlier one answers. The positive controls
/// record the admitted set sorted. Mutants: reorder two rules, drop one, or append before
/// admitting.
#[test]
fn each_hire_rule_refuses_in_order_and_journals_nothing() {
    let claude = |servers, code, message| Refused {
        provider: Provider::Claude,
        isolation: None,
        servers,
        code,
        message,
    };
    let cases = [
        claude(
            &["nope"],
            ErrorCode::InvalidParams,
            "agent.hire: MCP server `nope` is not declared in this daemon's [mcp.servers]",
        ),
        claude(
            &["memory", "memory"],
            ErrorCode::InvalidParams,
            "agent.hire: MCP server `memory` is named more than once",
        ),
        Refused {
            provider: Provider::Pi,
            ..claude(
                &["memory", "nope"],
                ErrorCode::InvalidParams,
                "agent.hire: MCP server `nope` is not declared in this daemon's [mcp.servers]",
            )
        },
        Refused {
            provider: Provider::Pi,
            ..claude(
                &["notes"],
                ErrorCode::InvalidParams,
                "agent.hire: MCP server `notes` is a program server, and this engine loads only \
                 http servers, because a program it starts would outlive the turn",
            )
        },
        Refused {
            isolation: Some(IsolationMode::Default),
            ..claude(
                &["notes"],
                ErrorCode::InvalidParams,
                "agent.hire: declared MCP servers load only under the default isolation; omit \
                 `isolation`",
            )
        },
        Refused {
            provider: Provider::Codex,
            ..claude(
                &["notes", "search"],
                ErrorCode::InvalidParams,
                "agent.hire: MCP server `notes` is a program server, and this engine loads only \
                 http servers, because a program it starts would outlive the turn",
            )
        },
        claude(
            &["notes", "search"],
            ErrorCode::Refused,
            "agent.hire: MCP server `search` needs `SEARCH_KEY`, which was unset or empty in the \
             daemon's environment when it started; set it and restart the daemon",
        ),
        claude(
            &["memory", "notes"],
            ErrorCode::Refused,
            "agent.hire: MCP server `notes`: a declared program server starts in this \
             repository's directory, where launchers such as npx read configuration; trust the \
             repository or declare an http server",
        ),
    ];
    let mut daemon = boot();
    for case in &cases {
        let error = hire(&daemon, |params| {
            params.provider = case.provider;
            params.isolation = case.isolation;
            params.mcp_servers = names(case.servers);
        })
        .expect_err("refused");
        assert_eq!(
            (error.code, error.message.as_str()),
            (case.code.code(), case.message)
        );
    }
    assert_eq!(
        hired_count(&daemon.fixture),
        0,
        "no refusal journals a hire"
    );

    daemon.running.stop(ExitCause::Ended);
}

/// The controls for the refusals above: an admitted hire records its set sorted. Mutant: record
/// the names in the order typed.
#[test]
fn an_admitted_hire_records_its_servers_sorted() {
    let mut daemon = boot();
    let claude = hire(&daemon, |params| {
        params.trusted = true;
        params.mcp_servers = names(&["notes", "memory"]);
    })
    .expect("a trusted Claude hire takes a program server");
    let codex = hire(&daemon, |params| {
        params.provider = Provider::Codex;
        params.mcp_servers = names(&["memory"]);
    })
    .expect("a Codex hire takes an http server");
    let recorded: Vec<(AgentId, Vec<McpServerName>)> = daemon
        .fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::AgentHired {
                id, mcp_servers, ..
            } => Some((id, mcp_servers)),
            _ => None,
        })
        .collect();
    assert_eq!(
        recorded,
        [
            (claude, names(&["memory", "notes"])),
            (codex, names(&["memory"]))
        ]
    );
    daemon.running.stop(ExitCause::Ended);
}

fn await_spawns(host: &SharedHost, count: usize) -> Vec<LaunchSpec> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let spawned = host.spawned();
        if spawned.len() >= count {
            return spawned;
        }
        assert!(Instant::now() < deadline, "{count} launches");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn holds(spec: &LaunchSpec, value: &str) -> bool {
    spec.env
        .iter()
        .any(|var| var.expose_value().to_string_lossy().contains(value))
        || spec
            .argv
            .iter()
            .any(|part| part.to_string_lossy().contains(value))
}

/// Every file under `root`, read whole.
fn files(root: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("a readable directory") {
            let path = entry.expect("an entry").path();
            let kind = std::fs::symlink_metadata(&path)
                .expect("metadata")
                .file_type();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                let bytes = std::fs::read(&path).expect("a readable file");
                out.push((path, bytes));
            }
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// The boot split: the opted-in agent's managed launch holds each of its servers' secrets under
/// the name the engine reads, and nothing else the daemon starts does — not a second agent's
/// launch, not a predicate child, not a sign-in check — and no planted value reaches an argv, the
/// journal or any file under the state or runtime directory. An attached launch and a condense call
/// start from the same environment the second agent's launch does. Mutant: hand children the
/// environment from before the split, and the second agent's launch holds both secrets.
#[test]
fn only_the_opted_in_launch_receives_the_declared_secrets() {
    let mut daemon = boot();
    let opted = hire(&daemon, |p| {
        p.trusted = true;
        p.mcp_servers = names(&["memory", "notes"]);
    })
    .expect("hired");
    common::set_goal(
        &daemon.running.handle,
        common::goal_params(opted, &["/usr/bin/true"]),
    );
    let spawned = await_spawns(&daemon.host, 1);
    let other = hire(&daemon, |_| {}).expect("hired");
    common::set_goal(
        &daemon.running.handle,
        common::goal_params(other, &["/usr/bin/true"]),
    );
    let spawned = {
        let _ = spawned;
        await_spawns(&daemon.host, 2)
    };
    daemon.running.stop(ExitCause::Ended);

    let (first, second) = (&spawned[0], &spawned[1]);
    let value = |spec: &LaunchSpec, name: &str| {
        spec.env
            .iter()
            .find(|var| var.name() == name)
            .map(|var| var.expose_value().to_string_lossy().into_owned())
    };
    assert_eq!(
        value(first, "DUDES_MCP_MEMORY_HEADER_0").as_deref(),
        Some(MEMORY_AUTH)
    );
    assert_eq!(
        value(first, "NOTES_API_KEY").as_deref(),
        Some(NOTES_API_KEY)
    );
    assert_eq!(
        value(first, "MEMORY_AUTH"),
        None,
        "the operator's name stays behind"
    );
    assert_eq!(value(first, "UNRELATED").as_deref(), Some("kept"));
    assert_eq!(value(second, "UNRELATED").as_deref(), Some("kept"));
    for secret in [MEMORY_AUTH, NOTES_API_KEY] {
        assert!(!holds(second, secret), "a second agent's launch");
        assert!(
            first
                .argv
                .iter()
                .all(|part| !part.to_string_lossy().contains(secret)),
            "the opted-in argv"
        );
        for child in daemon.predicates.seen() {
            assert!(!holds(&child, secret), "a predicate or check child");
        }
        for root in [daemon.fixture.state.path(), daemon.fixture.runtime.path()] {
            for (path, bytes) in files(root) {
                assert!(!contains(&bytes, secret), "{}", path.display());
            }
        }
    }
    assert!(
        first.argv.iter().any(|part| part == "--mcp-config"),
        "the opted-in launch composes its servers"
    );
    assert!(second.argv.iter().all(|part| part != "--mcp-config"));
}

/// A program server's `{agent_dir}` argument reaches the engine as the agent's recorded working
/// directory, the launch's own `cwd`. Mutant: stop resolving, and the token is passed through.
#[test]
fn a_program_servers_folder_argument_is_the_agents_recorded_directory() {
    let mut daemon = boot();
    let agent = hire(&daemon, |p| {
        p.trusted = true;
        p.mcp_servers = names(&["folder"]);
    })
    .expect("hired");
    common::set_goal(
        &daemon.running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let spawned = await_spawns(&daemon.host, 1);
    daemon.running.stop(ExitCause::Ended);

    let launch = &spawned[0];
    let at = launch
        .argv
        .iter()
        .position(|part| part == "--mcp-config")
        .expect("the servers are composed");
    let config: serde_json::Value =
        serde_json::from_str(&launch.argv[at + 1].to_string_lossy()).expect("one JSON value");
    assert_eq!(
        config["mcpServers"]["folder"]["args"],
        serde_json::json!(["--root", launch.cwd.to_string_lossy()])
    );
}

fn edit(
    daemon: &Daemon,
    agent: AgentId,
    selected: Option<&[&str]>,
) -> Result<dudes_proto::rpc::agent::McpResult, RpcError> {
    match daemon
        .running
        .handle
        .ask(duded::ports::Ask::Mcp(dudes_proto::rpc::agent::McpParams {
            agent,
            mcp_servers: selected.map(names),
        }))? {
        duded::ports::Answered::Mcp(result) => Ok(result),
        other => panic!("agent.mcp answered {other:?}"),
    }
}

#[test]
fn desired_read_replace_noop_and_clear_are_durable_without_scheduling_a_turn() {
    let mut daemon = boot();
    let agent = hire(&daemon, |_| {}).unwrap();
    let before = daemon.fixture.records();
    assert_eq!(
        edit(&daemon, agent, None).unwrap(),
        dudes_proto::rpc::agent::McpResult {
            agent,
            mcp_servers: Vec::new(),
            changed: false,
        }
    );
    assert_eq!(daemon.fixture.records(), before);
    assert_eq!(
        edit(&daemon, agent, Some(&["memory"])).unwrap(),
        dudes_proto::rpc::agent::McpResult {
            agent,
            mcp_servers: names(&["memory"]),
            changed: true,
        }
    );
    let recorded = daemon.fixture.records();
    assert!(!edit(&daemon, agent, Some(&["memory"])).unwrap().changed);
    assert_eq!(daemon.fixture.records(), recorded);
    assert!(!edit(&daemon, agent, None).unwrap().changed);
    assert_eq!(
        edit(&daemon, agent, Some(&[])).unwrap(),
        dudes_proto::rpc::agent::McpResult {
            agent,
            mcp_servers: Vec::new(),
            changed: true,
        }
    );
    assert!(!edit(&daemon, agent, Some(&[])).unwrap().changed);
    assert_eq!(
        &daemon.fixture.records()[before.len()..],
        [
            Record::AgentMcpServersChanged {
                agent,
                mcp_servers: names(&["memory"])
            },
            Record::AgentMcpServersChanged {
                agent,
                mcp_servers: Vec::new()
            },
        ]
    );
    assert!(daemon.host.spawned().is_empty());
    daemon.running.stop(ExitCause::Ended);
}

#[test]
fn changed_edits_apply_the_hires_original_isolation_trust_engine_and_boot_secrets() {
    let mut daemon = boot();
    let ordinary = hire(&daemon, |_| {}).unwrap();
    let explicit = hire(&daemon, |params| {
        params.isolation = Some(IsolationMode::Default);
    })
    .unwrap();
    let codex = hire(&daemon, |params| params.provider = Provider::Codex).unwrap();
    let before = daemon.fixture.records();
    for (agent, selected, code, reason) in [
        (
            ordinary,
            &["nope"][..],
            ErrorCode::InvalidParams,
            "MCP server `nope` is not declared",
        ),
        (
            ordinary,
            &["memory", "memory"][..],
            ErrorCode::InvalidParams,
            "MCP server `memory` is named more than once",
        ),
        (
            explicit,
            &["memory"][..],
            ErrorCode::InvalidParams,
            "declared MCP servers load only under the default isolation",
        ),
        (
            codex,
            &["notes"][..],
            ErrorCode::InvalidParams,
            "MCP server `notes` is a program server",
        ),
        (
            ordinary,
            &["search"][..],
            ErrorCode::Refused,
            "MCP server `search` needs `SEARCH_KEY`",
        ),
        (
            ordinary,
            &["notes"][..],
            ErrorCode::Refused,
            "MCP server `notes`: a declared program server starts in this repository",
        ),
    ] {
        let error = edit(&daemon, agent, Some(selected)).unwrap_err();
        assert_eq!(error.code, code.code());
        assert!(
            error.message.starts_with(&format!("agent.mcp: {reason}")),
            "{}",
            error.message
        );
        assert!(!error.message.contains(MEMORY_AUTH));
        assert!(!error.message.contains(NOTES_API_KEY));
        assert_eq!(daemon.fixture.records(), before);
    }
    assert!(
        !edit(&daemon, explicit, Some(&[])).unwrap().changed,
        "empty repair needs no isolation admission"
    );
    assert!(edit(&daemon, ordinary, Some(&["memory"])).unwrap().changed);
    daemon.running.stop(ExitCause::Ended);
}

#[test]
fn a_failed_edit_keeps_the_initiating_error_and_fence_but_read_remains_available() {
    let mut daemon = boot();
    let agent = hire(&daemon, |_| {}).unwrap();
    let before = daemon.fixture.records();
    daemon.append_failure.short_write_next(5);
    let failure = edit(&daemon, agent, Some(&["memory"])).unwrap_err();
    assert_eq!(failure.code, ErrorCode::InternalError.code());
    assert!(
        failure
            .message
            .contains("appending a AgentMcpServersChanged"),
        "{}",
        failure.message
    );
    assert_eq!(
        daemon.fixture.records(),
        before,
        "the edit was never acknowledged or retried"
    );
    assert_eq!(
        edit(&daemon, agent, None).unwrap(),
        dudes_proto::rpc::agent::McpResult {
            agent,
            mcp_servers: Vec::new(),
            changed: false,
        }
    );
    for selected in [&["memory"][..], &[][..]] {
        assert_eq!(
            edit(&daemon, agent, Some(selected)).unwrap_err().code,
            dudes_proto::rpc::JOURNAL_STOPPED_CODE
        );
    }
    assert_eq!(daemon.fixture.records(), before);
    daemon.running.stop(ExitCause::Ended);
}

#[test]
fn mcp_reads_and_replacements_refuse_unknown_and_retired_agents() {
    let mut daemon = boot();
    let retired = hire(&daemon, |_| {}).unwrap();
    daemon
        .running
        .handle
        .ask(duded::ports::Ask::Retire(Box::new(
            dudes_proto::rpc::agent::RetireParams {
                agent: retired,
                reason: "fixture retirement".into(),
            },
        )))
        .unwrap();
    let before = daemon.fixture.records();
    for agent in [AgentId::NIL, retired] {
        for selected in [None, Some(&[][..])] {
            assert_eq!(
                edit(&daemon, agent, selected).unwrap_err().code,
                ErrorCode::NotFound.code()
            );
        }
    }
    assert_eq!(daemon.fixture.records(), before);
    daemon.running.stop(ExitCause::Ended);
}
