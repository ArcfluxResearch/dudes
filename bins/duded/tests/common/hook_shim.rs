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

/// Every test process builds the shim. On macOS, Cargo copies each executable out of its build
/// tree instead of hard-linking it, so every build removes that copy and writes it again, even
/// when it compiles nothing. A process that looks in that moment finds no file. So the shim builds
/// in a target directory of its own, and the workspace's `target/debug/dudes-hook` stays with the
/// tests that run it. These tests run a copy named by its content, which no later build replaces,
/// and the lock stops another test process from rebuilding while that copy is taken. A shim a
/// workspace build already left beside the test executables is copied instead, with no Cargo run.
fn publish_hook() -> PathBuf {
    if let Some(built) = prebuilt_hook() {
        return copy_by_content(&built);
    }
    let dir = option_env!("CARGO_TARGET_TMPDIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp"),
        PathBuf::from,
    );
    let lock = std::fs::File::create(dir.join("dudes-hook.lock")).expect("open the shim lock");
    lock.lock().expect("take the shim lock");
    let built = build_hook(&dir.join("hook-target"));
    copy_by_content_in(&dir, &built)
}

/// The shim a workspace build already produced beside this test's own executable, when it is at
/// least as new as every source file that can change it. Building it again from inside a test costs
/// a Cargo run per test process, and a cold first run holds every other test behind the lock for
/// as long as the compile takes.
fn prebuilt_hook() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .join(format!("dudes-hook{}", std::env::consts::EXE_SUFFIX));
    let built = std::fs::metadata(&beside).ok()?.modified().ok()?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?;
    let mut newest = std::time::SystemTime::UNIX_EPOCH;
    let mut pending = vec![root.join("bins/dudes-hook/src"), root.join("crates")];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "tests") {
                    pending.push(path);
                }
            } else if path
                .extension()
                .is_some_and(|ext| ext == "rs" || ext == "toml")
            {
                newest = newest.max(entry.metadata().ok()?.modified().ok()?);
            }
        }
    }
    (built >= newest).then_some(beside)
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

#[allow(
    clippy::disallowed_types,
    reason = "the test harness builds the sibling shim binary named by the accepted end-to-end check; daemon src/ remains guarded by host_seam.rs"
)]
fn build_hook(target_dir: &Path) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("bins/duded is two levels below the workspace root");
    let output = std::process::Command::new(env!("CARGO"))
        .current_dir(root)
        .env("CARGO_BUILD_JOBS", "2")
        .args([
            "build",
            "--locked",
            "-p",
            "dudes-hook",
            "--bin",
            "dudes-hook",
            "--message-format=json",
        ])
        .arg("--target-dir")
        .arg(target_dir)
        .output()
        .expect("run Cargo for the sibling shim");
    assert!(
        output.status.success(),
        "building dudes-hook failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("Cargo JSON is UTF-8")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|message| {
            (message["reason"] == "compiler-artifact" && message["target"]["name"] == "dudes-hook")
                .then(|| message["executable"].as_str().map(PathBuf::from))
                .flatten()
        })
        .expect("Cargo reported the dudes-hook executable")
}

pub(crate) fn generated_handlers(agent: AgentId, agent_dir: &Path) -> Settings {
    handlers_for(agent, agent_dir, hook_binary())
}

pub(crate) fn handlers_for(agent: AgentId, agent_dir: &Path, hook_command: &Path) -> Settings {
    let adapter = ClaudeAdapter::default();
    let ctx = AgentCtx {
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
