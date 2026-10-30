//! an adapter's `program()` is the program its managed launch runs, so the daemon's
//! provider check reports the file that launch would start.

use std::ffi::OsString;
use std::path::Path;

use dudes_providers::adapter::{Adapter, AgentCtx, TurnSession};
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::codex::CodexAdapter;
use dudes_providers::codex::config::CodexConfig;
use dudes_providers::pi::PiAdapter;
use dudes_providers::pi::config::PiConfig;
use dudes_types::{AgentId, ModelSelection, SessionId};

#[test]
fn each_adapter_reports_the_program_its_managed_launch_runs() {
    let adapters: [(Box<dyn Adapter>, &str); 3] = [
        (
            Box::new(ClaudeAdapter::new(ClaudeConfig {
                command: "/opt/tools/claude-configured".into(),
                ..ClaudeConfig::default()
            })),
            "/opt/tools/claude-configured",
        ),
        (
            Box::new(CodexAdapter::new(CodexConfig {
                streaming: false,
                command: "codex-configured".to_owned(),
            })),
            "codex-configured",
        ),
        (
            Box::new(PiAdapter::new(PiConfig {
                command: "/opt/tools/pi-configured".to_owned(),
            })),
            "/opt/tools/pi-configured",
        ),
    ];
    let ctx = AgentCtx {
        extra_dirs: &[],
        agent: AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a ulid"),
        cwd: Path::new("/repo"),
        agent_dir: Path::new("/var/lib/dudes/agents/01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        daemon_env: &[],
        sandbox: None,
        trusted: false,
        max_turns: None,
        max_budget: None,
        mcp_servers: &[],
    };
    for (adapter, configured) in adapters {
        let provider = adapter.provider();
        let spec = adapter
            .managed(
                &ctx,
                "make the tests pass",
                &TurnSession::Resume(SessionId::from("vendor-session")),
                &ModelSelection::provider_default(),
            )
            .unwrap_or_else(|error| panic!("{provider}: a launch: {error:?}"));
        assert_eq!(
            adapter.program(),
            configured,
            "{provider}: the configured command"
        );
        assert_eq!(
            spec.argv.first().map(OsString::as_os_str),
            Some(adapter.program()),
            "{provider}: the program the launch runs"
        );
    }
}
