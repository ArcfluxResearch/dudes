//! The generated Claude hook command, built and spawned for real: the helpers the hook-route and
//! Floor-action tests share, so a hook's end-to-end encoding is asserted through one harness.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use dudes_proto::Record;
use dudes_proto::providers::claude::{HookHandler, Settings};
use dudes_proto::rpc::hook::HookToken;
use dudes_providers::adapter::{Adapter, AgentCtx};
use dudes_providers::claude::ClaudeAdapter;
use dudes_rpc::auth::{PeerIdentity, PeerProvenance, PeerResolution, PeerResolver};
use dudes_types::AgentId;
use serde_json::{Value, json};

use super::Fixture;

pub(crate) const PROCESS_WAIT: Duration = Duration::from_secs(10);
static HOOK_BINARY: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug)]
pub(crate) struct RecordingResolver {
    provenance: Mutex<PeerProvenance>,
    peers: Mutex<Vec<PeerIdentity>>,
}

impl RecordingResolver {
    pub(crate) fn new(provenance: PeerProvenance) -> Arc<Self> {
        Arc::new(Self {
            provenance: Mutex::new(provenance),
            peers: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn set(&self, provenance: PeerProvenance) {
        *self.provenance.lock().expect("resolver provenance") = provenance;
    }

    pub(crate) fn pids(&self) -> Vec<i32> {
        self.peers
            .lock()
            .expect("recorded peers")
            .iter()
            .filter_map(|peer| peer.pid.reported())
            .collect()
    }
}

impl PeerResolver for RecordingResolver {
    fn resolve(&self, peer: &PeerIdentity) -> PeerResolution {
        self.peers.lock().expect("recorded peers").push(*peer);
        PeerResolution::Resolved(*self.provenance.lock().expect("resolver provenance"))
    }
}

pub(crate) fn hook_binary() -> &'static Path {
    HOOK_BINARY.get_or_init(publish_hook).as_path()
}

fn publish_hook() -> PathBuf {
    let built = std::env::current_exe()
        .expect("this test executable")
        .parent()
        .and_then(Path::parent)
        .expect("the test executable is under the build directory")
        .join(format!("dudes-hook{}", std::env::consts::EXE_SUFFIX));
    assert!(
        built.is_file(),
        "{} is absent; run `cargo build --locked -p dudes-hook` before a filtered test run",
        built.display()
    );
    copy_by_content(&built)
}

fn copy_by_content(built: &Path) -> PathBuf {
    let dir = option_env!("CARGO_TARGET_TMPDIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp"),
        PathBuf::from,
    );
    copy_by_content_in(&dir, built)
}

fn copy_by_content_in(dir: &Path, built: &Path) -> PathBuf {
    let digest = dudes_supervisor::blake3_hex(&std::fs::read(built).expect("read the built shim"));
    let published = dir.join(format!(
        "dudes-hook-{digest}{}",
        std::env::consts::EXE_SUFFIX
    ));
    if !published.exists() {
        let staged = published.with_extension(format!("partial{}", std::process::id()));
        std::fs::copy(built, &staged).expect("stage the shim");
        std::fs::rename(&staged, &published).expect("publish the shim");
    }
    published
}

pub(crate) fn generated_handlers(agent: AgentId, agent_dir: &Path) -> Settings {
    handlers_for(agent, agent_dir, hook_binary())
}

pub(crate) fn handlers_for(agent: AgentId, agent_dir: &Path, hook_command: &Path) -> Settings {
    let adapter = ClaudeAdapter::default();
    let ctx = AgentCtx {
        extra_dirs: &[],
        agent,
        cwd: agent_dir,
        agent_dir,
        daemon_env: &[],
        sandbox: None,
        trusted: false,
        max_turns: None,
        max_budget: None,
        mcp_servers: &[],
    };
    let install = adapter.install_hooks(&ctx, hook_command);
    assert_eq!(install.files.len(), 1, "one generated settings file");
    serde_json::from_str(&install.files[0].1).expect("generated Claude hook settings")
}

pub(crate) fn handler<'a>(settings: &'a Settings, event: &str) -> &'a HookHandler {
    let matchers = settings
        .hooks
        .get(event)
        .expect("literal modeled event key");
    assert_eq!(matchers.len(), 1, "{event}: one matcher group");
    assert_eq!(matchers[0].hooks.len(), 1, "{event}: one handler");
    let handler = &matchers[0].hooks[0];
    assert_eq!(handler.kind, "command", "{event}: direct exec handler");
    assert!(
        handler.args.as_deref().is_some_and(<[String]>::is_empty),
        "{event}: empty argv"
    );
    assert!(handler.url.is_none(), "{event}: no HTTP handler URL");
    handler
}

pub(crate) fn payload(event: &str) -> Value {
    match event {
        "PreToolUse" => json!({
            "hook_event_name": event,
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test"},
            "agent_id": "payload-selected-identity-is-ignored"
        }),
        "PostToolUse" => json!({
            "hook_event_name": event,
            "tool_name": "Edit",
            "tool_response": {"ok": true}
        }),
        "Stop" => json!({"hook_event_name": event, "last_assistant_message": "done"}),
        "Notification" => json!({"hook_event_name": event, "message": "permission needed"}),
        _ => json!({"hook_event_name": event}),
    }
}

#[allow(
    clippy::disallowed_types,
    reason = "the test harness invokes the exact generated hook command; daemon src/ remains guarded by host_seam.rs"
)]
pub(crate) fn spawn_handler(
    handler: &HookHandler,
    socket: &Path,
    token: &HookToken,
    raw: &Value,
) -> (u32, Output) {
    let command = handler.command.as_deref().expect("command path");
    let mut process = std::process::Command::new(command);
    process
        .args(handler.args.as_deref().expect("exec-form args"))
        .env("DUDES_SOCK", socket)
        .env("DUDES_TOKEN", token.expose())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process.spawn().expect("spawn generated hook command");
    let pid = child.id();
    child
        .stdin
        .take()
        .expect("hook stdin")
        .write_all(&serde_json::to_vec(raw).unwrap())
        .expect("write hook payload");
    (pid, wait_bounded(child))
}

pub(crate) fn wait_bounded(mut child: Child) -> Output {
    let deadline = Instant::now() + PROCESS_WAIT;
    while child.try_wait().expect("poll hook child").is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("hook child did not exit within {PROCESS_WAIT:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    child.wait_with_output().expect("collect hook output")
}

pub(crate) fn minted_token(
    fixture: &Fixture,
    tokens: &dudes_supervisor::testing::FakeTokens,
    agent: AgentId,
) -> HookToken {
    let hash = fixture.await_record("HookTokenMinted", |record| match record {
        Record::HookTokenMinted {
            agent: owner,
            token_hash,
        } if *owner == agent => Some(token_hash.clone()),
        _ => None,
    });
    tokens.token_of(&hash)
}
