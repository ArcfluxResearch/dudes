//! Authenticated, run-local publication from two real daemon-owned Managed children.
#![cfg(unix)]

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use duded::floor_browser::Page;
use duded::floor_stream::FloorStreamFault;
use duded::settings::{FloorSettings, Seams};
use duded::startup::Witness;
use duded::{ChildEnds, ChildTurn, ExitCause};
use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_proto::ExitStatus;
use dudes_providers::adapter::{
    Adapter, AgentCtx, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
};
use dudes_providers::claude::SESSION_ACQUISITION;
use dudes_providers::claude::stream::TurnStream;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_providers::stream::TurnReader;
use dudes_supervisor::Clock as _;
use dudes_supervisor::host::AdoptionKey;
use dudes_supervisor::testing::FakeRng;
use dudes_types::{ModelSelection, Provider, Tier};
use serde_json::Value;

mod common;
use common::{Fixture, ScriptedRunner, TestClock};

const HELPER: &str = "DUDES_FLOOR_OUTPUT_HELPER";
const LABEL: &str = "DUDES_FLOOR_OUTPUT_LABEL";
const SESSION: &str = "DUDES_FLOOR_OUTPUT_SESSION";
const DIR: &str = "DUDES_FLOOR_OUTPUT_DIR";
const STEP_SECRET: &str = "STEPS_API_TOKEN";
const SECRET_VALUE: &str = "synthetic-steps-token-0123456789abcdef";
const ASSETS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v7"
);

#[test]
fn managed_output_child_helper() {
    if std::env::var_os(HELPER).is_none() {
        return;
    }
    let label = std::env::var(LABEL).expect("child label");
    let session = std::env::var(SESSION).expect("child session");
    let dir = PathBuf::from(std::env::var_os(DIR).expect("child directory"));
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        r#"{{"type":"system","subtype":"init","session_id":"{session}","tools":[]}}"#
    )
    .expect("init line");
    assistant(&mut out, &session, &format!("{label}-first"));
    out.flush().expect("first output flush");
    std::fs::write(dir.join("first-ready"), b"ready").expect("first marker");
    if label == "FLOOD" {
        for index in 0..300 {
            wait_file(&dir.join(format!("release-chunk-{index}")));
            assistant(&mut out, &session, &format!("FLOOD-{index}"));
            out.flush().expect("flood output flush");
            std::fs::write(dir.join(format!("wrote-chunk-{index}")), b"ready")
                .expect("flood marker");
        }
        wait_file(&dir.join("release-exit"));
        return;
    }
    if label == "STEPS" {
        let token = std::env::var(STEP_SECRET).expect("planted secret");
        let lines = [
            format!(
                r#"{{"type":"assistant","session_id":"{session}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_curl","name":"Bash","input":{{"command":"curl -H 'Authorization: Bearer {token}' https://example.test"}}}},{{"type":"tool_use","id":"toolu_aws","name":"Read","input":{{"file_path":"/work/user/.aws/credentials"}}}},{{"type":"tool_use","id":"toolu_edit","name":"Edit","input":{{"file_path":"/repo/src/a.rs","old_string":"let key = \"{token}\";","new_string":"let key = env();"}}}},{{"type":"tool_use","id":"toolu_plan","name":"TodoWrite","input":{{"todos":[{{"content":"Rotate {token}","status":"in_progress","activeForm":"Rotating"}}]}}}}]}}}}"#
            ),
            format!(
                r#"{{"type":"user","session_id":"{session}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_curl","content":"ok","is_error":false}}]}}}}"#
            ),
        ];
        for line in lines {
            writeln!(out, "{line}").expect("step line");
        }
        out.flush().expect("step flush");
        std::fs::write(dir.join("steps-ready"), b"ready").expect("steps marker");
        wait_file(&dir.join("release-exit"));
        writeln!(out, r#"{{"type":"result","session_id":"{session}","subtype":"success","is_error":false,"result":"done"}}"#).expect("result line");
        out.flush().expect("end flush");
        return;
    }
    if label == "RESULTS" {
        results_child(&mut out, &session, &dir);
        return;
    }
    if label == "CODEX" || label == "PI" {
        engine_child(&mut out, &label, &session, &dir);
        return;
    }
    wait_file(&dir.join("release-next"));
    assistant(&mut out, &session, &format!("{label}-second"));
    writeln!(out, r#"{{"type":"assistant","session_id":"wrong","message":{{"role":"assistant","content":[{{"type":"text","text":"{label}-wrong-session"}}]}}}}"#).expect("wrong-session line");
    writeln!(out, r#"{{"type":"assistant","session_id":"{session}","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"{label}-thought"}}]}}}}"#).expect("thinking line");
    out.flush().expect("second output flush");
    std::fs::write(dir.join("second-ready"), b"ready").expect("second marker");
    if label == "A" {
        wait_file(&dir.join("release-third"));
        assistant(&mut out, &session, "A-third");
        out.flush().expect("third output flush");
        std::fs::write(dir.join("third-ready"), b"ready").expect("third marker");
    }
    wait_file(&dir.join("release-exit"));
    writeln!(out, r#"{{"type":"result","session_id":"{session}","subtype":"success","is_error":false,"result":"{label}-result"}}"#).expect("result line");
    out.flush().expect("end flush");
}

#[allow(clippy::too_many_lines)]
fn results_child(out: &mut impl std::io::Write, session: &str, dir: &Path) {
    let token = std::env::var(STEP_SECRET).expect("planted secret");
    let call = |id: &str, name: &str, input: Value| {
        serde_json::json!({"type": "assistant", "session_id": session, "message": {
            "role": "assistant",
            "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}})
    };
    let result = |id: &str, text: &str| {
        serde_json::json!({"type": "user", "session_id": session, "message": {
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": id, "content": text,
                "is_error": false}]}})
    };
    let bash = |command: &str| serde_json::json!({ "command": command });
    let mut lines = Vec::new();
    for index in 0..70 {
        let id = format!("toolu_fill{index}");
        lines.push(call(&id, "Bash", bash("true")));
        lines.push(result(&id, &format!("filler {index}")));
    }
    let big = format!("{}TAIL-MARK", "x".repeat(20_000));
    for (id, name, input, text) in [
        (
            "toolu_plain",
            "Bash",
            bash("echo plain"),
            "plain output line".to_owned(),
        ),
        (
            "toolu_known",
            "Bash",
            bash("echo x"),
            format!("token {token} end"),
        ),
        (
            "toolu_shape",
            "Bash",
            bash("echo y"),
            "key=ghp_abcdefghijklmnopqrstuvwxyz0123456789".to_owned(),
        ),
        (
            "toolu_env",
            "Read",
            serde_json::json!({"file_path": "/work/user/.env"}),
            "DB_URL=PRIVATE-ENV-TEXT".to_owned(),
        ),
        (
            "toolu_cat",
            "Bash",
            bash("cat ~/.aws/credentials"),
            "PRIVATE-AWS-TEXT".to_owned(),
        ),
        (
            "toolu_mcp",
            "mcp__fs__read_file",
            serde_json::json!({"path": "/work/a.txt"}),
            "PRIVATE-MCP-TEXT".to_owned(),
        ),
        (
            "toolu_bgout",
            "BashOutput",
            serde_json::json!({"bash_id": "bash_1"}),
            "PRIVATE-BG-TEXT".to_owned(),
        ),
        (
            "toolu_grep",
            "Grep",
            serde_json::json!({"pattern": "KEY", "path": "/work/user"}),
            "config/.aws/credentials:3:PRIVATE-GREP-TEXT".to_owned(),
        ),
        (
            "toolu_mystery",
            "Mystery",
            serde_json::json!({"note": "x"}),
            "PRIVATE-OTHER-TEXT".to_owned(),
        ),
        (
            "toolu_envfile",
            "Bash",
            bash("cat settings.ini"),
            "DB_PASSWORD=hunter2hunter2".to_owned(),
        ),
        (
            "toolu_numbered",
            "Read",
            serde_json::json!({"file_path": "config/settings.ini"}),
            "     3\u{2192}password = hunter2hunter2".to_owned(),
        ),
        (
            "toolu_secretedit",
            "Edit",
            serde_json::json!({"file_path": "/repo/c.rs", "old_string": "a",
                "new_string": format!("password = hunter2hunter2\n{}", "n".repeat(5_000))}),
            "updated".to_owned(),
        ),
        ("toolu_big", "Bash", bash("cat big.txt"), big),
        (
            "toolu_edit",
            "Edit",
            serde_json::json!({"file_path": "/repo/a.rs", "old_string": "a",
                "new_string": "n".repeat(5_000)}),
            "updated".to_owned(),
        ),
        (
            "toolu_write",
            "Write",
            serde_json::json!({"file_path": "/repo/b.rs", "content": "w".repeat(9_000)}),
            "created".to_owned(),
        ),
    ] {
        lines.push(call(id, name, input));
        lines.push(result(id, &text));
    }
    for line in lines {
        writeln!(out, "{line}").expect("step line");
    }
    out.flush().expect("step flush");
    std::fs::write(dir.join("steps-ready"), b"ready").expect("steps marker");
    wait_file(&dir.join("release-exit"));
    writeln!(out, r#"{{"type":"result","session_id":"{session}","subtype":"success","is_error":false,"result":"done"}}"#).expect("result line");
    out.flush().expect("end flush");
}

/// The lines a Codex exec run and a Pi run write for a few tool calls, in each engine's own shape.
fn engine_child(out: &mut impl std::io::Write, label: &str, session: &str, dir: &Path) {
    let codex = label == "CODEX";
    let lines = if codex {
        let item = |kind: &str, item: Value| {
            let mut item = item;
            item["type"] = kind.into();
            item
        };
        let line = |kind: &str, value: Value| serde_json::json!({"type": kind, "item": value});
        let files = serde_json::json!([
            {"path": "/repo/new.rs", "kind": "add"}, {"path": "/repo/a.rs", "kind": "update"}]);
        vec![
            line("item.started", item("command_execution", serde_json::json!({"id": "item_1", "command": "/bin/zsh -lc 'ls'", "aggregated_output": "", "exit_code": null, "status": "in_progress"}))),
            line("item.completed", item("command_execution", serde_json::json!({"id": "item_1", "command": "/bin/zsh -lc 'ls'", "aggregated_output": "a.txt\n", "exit_code": 0, "status": "completed"}))),
            line("item.started", item("command_execution", serde_json::json!({"id": "item_2", "command": "/bin/zsh -lc 'cat ~/.aws/credentials'", "aggregated_output": "", "exit_code": null, "status": "in_progress"}))),
            line("item.completed", item("command_execution", serde_json::json!({"id": "item_2", "command": "/bin/zsh -lc 'cat ~/.aws/credentials'", "aggregated_output": "PRIVATE-AWS-TEXT", "exit_code": 0, "status": "completed"}))),
            line("item.started", item("command_execution", serde_json::json!({"id": "item_3", "command": "false", "status": "in_progress"}))),
            line("item.completed", item("command_execution", serde_json::json!({"id": "item_3", "command": "false", "aggregated_output": "nope", "exit_code": 2, "status": "failed"}))),
            line("item.started", item("file_change", serde_json::json!({"id": "item_4", "changes": files, "status": "in_progress"}))),
            line("item.completed", item("file_change", serde_json::json!({"id": "item_4", "changes": files, "status": "completed"}))),
            line("item.started", item("todo_list", serde_json::json!({"id": "item_5", "items": [{"text": "Ship it", "completed": false}]}))),
            line("item.completed", item("todo_list", serde_json::json!({"id": "item_5", "items": [{"text": "Ship it", "completed": true}]}))),
        ]
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
    } else {
        let start = |id: &str, tool: &str, args: Value| serde_json::json!({"type": "tool_execution_start", "toolCallId": id, "toolName": tool, "args": args});
        let end = |id: &str, tool: &str, text: &str, error: bool| serde_json::json!({"type": "tool_execution_end", "toolCallId": id, "toolName": tool, "result": {"content": [{"type": "text", "text": text}]}, "isError": error});
        vec![
            start("call_1", "bash", serde_json::json!({"command": "ls"})),
            end("call_1", "bash", "a.txt\n", false),
            start("call_2", "read", serde_json::json!({"path": "/work/user/.env"})),
            end("call_2", "read", "PRIVATE-ENV-TEXT", false),
            start("call_3", "grep", serde_json::json!({"pattern": "todo", "path": "src"})),
            end("call_3", "grep", "no match", true),
            start("call_4", "edit", serde_json::json!({"path": "/repo/a.rs", "edits": [{"oldText": "let a = 1;", "newText": "let a = 2;"}]})),
            end("call_4", "edit", "ok", false),
        ]
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
    };
    if codex {
        writeln!(
            out,
            r#"{{"type":"thread.started","thread_id":"{session}"}}"#
        )
        .expect("thread line");
    } else {
        writeln!(out, r#"{{"type":"session","version":3,"id":"{session}"}}"#).expect("header line");
    }
    for line in lines {
        writeln!(out, "{line}").expect("step line");
    }
    out.flush().expect("step flush");
    std::fs::write(dir.join("steps-ready"), b"ready").expect("steps marker");
    wait_file(&dir.join("release-exit"));
}

fn assistant(out: &mut impl std::io::Write, session: &str, text: &str) {
    writeln!(out, r#"{{"type":"assistant","session_id":"{session}","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#).expect("assistant line");
}

fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "child wait timed out: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct LocalAdapter;
impl Adapter for LocalAdapter {
    fn provider(&self) -> Provider {
        Provider::Claude
    }
    fn program(&self) -> &std::ffi::OsStr {
        std::ffi::OsStr::new("claude")
    }
    fn tier(&self) -> Tier {
        Tier::HookGoverned
    }
    fn isolation(&self) -> Isolation {
        Isolation::None
    }
    fn session_acquisition(&self) -> SessionAcquisition {
        SESSION_ACQUISITION
    }
    fn turn_reader(&self) -> Box<dyn TurnReader> {
        Box::new(TurnStream::new())
    }
    fn turn_reader_for(&self, spec: &LaunchSpec) -> Box<dyn TurnReader> {
        let label = spec
            .env
            .iter()
            .find(|var| var.name() == LABEL)
            .map(|var| var.expose_value().to_string_lossy().into_owned());
        match label.as_deref() {
            Some("CODEX") => Box::new(dudes_providers::codex::stream::TurnStream::new()),
            Some("PI") => Box::new(dudes_providers::pi::stream::TurnStream::new()),
            _ => self.turn_reader(),
        }
    }
    fn model_selection(&self) -> ModelSelection {
        ModelSelection::provider_default()
    }
    fn managed(
        &self,
        ctx: &AgentCtx<'_>,
        _task: &str,
        session: &TurnSession,
        _selection: &ModelSelection,
    ) -> Result<LaunchSpec, dudes_providers::launch::LaunchError> {
        let session = session
            .id()
            .ok_or(dudes_providers::launch::LaunchError::SessionModeMismatch)?;
        Ok(LaunchSpec {
            argv: [
                OsString::from(std::env::current_exe().expect("test binary")),
                "--exact".into(),
                "managed_output_child_helper".into(),
                "--quiet".into(),
            ]
            .into(),
            env: [
                EnvVar::new(HELPER, "1"),
                EnvVar::new(LABEL, ctx.cwd.file_name().expect("child label")),
                EnvVar::new(SESSION, session.as_str()),
                EnvVar::new(DIR, ctx.cwd.as_os_str()),
                EnvVar::new(STEP_SECRET, SECRET_VALUE),
            ]
            .into(),
            cwd: ctx.cwd.to_path_buf(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        })
    }
    fn managed_install(&self, _ctx: &AgentCtx<'_>, _selection: &ModelSelection) -> ManagedInstall {
        ManagedInstall::default()
    }
    fn usage_source(&self) -> UsageSource {
        UsageSource::HeadlessResult
    }
    fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
        None
    }
    fn billing_env_vars(&self) -> &'static [&'static str] {
        &[]
    }
}

/// Child lines the daemon has finished reading. A child's marker file says only that it wrote a
/// line; a line recorded here has already been captured into the daemon's output registry.
#[derive(Default)]
struct Captured {
    lines: Mutex<Vec<String>>,
    changed: Condvar,
}
impl Captured {
    fn wait_for(&self, text: &str) {
        let lines = self.lines.lock().unwrap();
        let (lines, _) = self
            .changed
            .wait_timeout_while(lines, Duration::from_secs(20), |lines| {
                !lines.iter().any(|line| line.contains(text))
            })
            .unwrap();
        assert!(
            lines.iter().any(|line| line.contains(text)),
            "the daemon never read {text}"
        );
    }
}

struct HostStreams(Arc<ChildEnds>, Arc<Captured>);
impl ChildStreams for HostStreams {
    fn watch(&self, key: Option<&AdoptionKey>) -> Box<dyn ChildStream> {
        Box::new(HostChildTurn(self.0.watch(key), Arc::clone(&self.1)))
    }
}
struct HostChildTurn(ChildTurn, Arc<Captured>);
impl ChildStream for HostChildTurn {
    fn line(&mut self, text: &str) {
        self.0.line(text);
        self.1.lines.lock().unwrap().push(text.to_owned());
        self.1.changed.notify_all();
    }
    fn ended(self: Box<Self>, exit: ChildExit) {
        self.0.ended_with_elapsed(
            ExitStatus {
                code: exit.code,
                signal: exit.signal,
            },
            exit.elapsed_ms,
        );
    }
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn settings(fixture: &Fixture, port: u16) -> duded::Settings {
    let mut settings = fixture.settings();
    settings.floor = Some(FloorSettings {
        assets_dir: ASSETS.into(),
        port: Some(port),
    });
    settings
}

fn seams(
    settings: &duded::Settings,
    children: Arc<ChildEnds>,
    captured: Arc<Captured>,
    clock: Arc<TestClock>,
) -> Seams<RealHost, ScriptedRunner, TestClock, common::FixtureRng> {
    Seams {
        host: RealHost::watching(Arc::new(HostStreams(Arc::clone(&children), captured))),
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates: Arc::new(ScriptedRunner::scripted([
            common::passes(),
            common::passes(),
        ])),
        clock,
        rng: common::FixtureRng::from_rng(settings, FakeRng::new()),
        resolver: duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk)),
        adapters: [Box::new(LocalAdapter) as Box<dyn Adapter>]
            .into_iter()
            .collect(),
        children,
        witness: Witness::default(),
    }
}

fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("HTTP head");
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

struct WebSocket(TcpStream);
impl WebSocket {
    fn connect(page: &Page) -> Self {
        let mut stream = TcpStream::connect(page.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .write_all(
                page.upgrade_head("/floor/stream", "dudes-floor-v4")
                    .as_bytes(),
            )
            .unwrap();
        let head = read_head(&mut stream);
        assert!(
            head.starts_with("HTTP/1.1 101 Switching Protocols"),
            "{head}"
        );
        Self(stream)
    }
    fn send(&mut self, text: &str) {
        let payload = text.as_bytes();
        let mut frame = vec![0x81];
        match payload.len() {
            0..=125 => frame.push(0x80 | u8::try_from(payload.len()).unwrap()),
            126..=65_535 => {
                frame.push(0x80 | 0x7e);
                frame.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
            }
            _ => {
                frame.push(0x80 | 0x7f);
                frame.extend_from_slice(&u64::try_from(payload.len()).unwrap().to_be_bytes());
            }
        }
        let mask = [1_u8, 2, 3, 4];
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(i, byte)| byte ^ mask[i % 4]),
        );
        self.0.write_all(&frame).unwrap();
    }
    fn json(&mut self) -> Value {
        let mut head = [0_u8; 2];
        self.0.read_exact(&mut head).expect("WebSocket head");
        assert_eq!(head[0] & 0x0f, 1, "expected text frame");
        let len = match head[1] & 0x7f {
            n @ 0..=125 => usize::from(n),
            126 => {
                let mut bytes = [0; 2];
                self.0.read_exact(&mut bytes).unwrap();
                usize::from(u16::from_be_bytes(bytes))
            }
            _ => {
                let mut bytes = [0; 8];
                self.0.read_exact(&mut bytes).unwrap();
                usize::try_from(u64::from_be_bytes(bytes)).unwrap()
            }
        };
        let mut body = vec![0; len];
        self.0.read_exact(&mut body).expect("WebSocket body");
        serde_json::from_slice(&body).expect("server JSON")
    }
    fn try_json(&mut self) -> Option<Value> {
        let mut head = [0_u8; 2];
        self.0.read_exact(&mut head).ok()?;
        let len = match head[1] & 0x7f {
            n @ 0..=125 => usize::from(n),
            126 => {
                let mut bytes = [0; 2];
                self.0.read_exact(&mut bytes).ok()?;
                usize::from(u16::from_be_bytes(bytes))
            }
            _ => {
                let mut bytes = [0; 8];
                self.0.read_exact(&mut bytes).ok()?;
                usize::try_from(u64::from_be_bytes(bytes)).ok()?
            }
        };
        let mut body = vec![0; len];
        self.0.read_exact(&mut body).ok()?;
        serde_json::from_slice(&body).ok()
    }
    fn output(&mut self) -> Value {
        loop {
            let frame = self.json();
            if frame["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("floor.output."))
            {
                return frame;
            }
        }
    }
    fn text(&mut self, expected: &str) -> Value {
        for _ in 0..12 {
            let frame = self.output();
            if frame.to_string().contains(expected) {
                return frame;
            }
            assert!(!frame.to_string().contains("wrong-session"));
        }
        panic!("selected output never contained {expected}");
    }
}

fn read_transport_close(
    stream: &mut TcpStream,
    deadline: Instant,
    byte_cap: usize,
) -> Result<usize, String> {
    let mut buffer = [0; 8192];
    let mut drained = 0;
    let mut first_byte = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "transport stayed open to deadline; drained_bytes={drained}; first_byte={first_byte:?}"
            ));
        }
        stream.set_read_timeout(Some(remaining)).map_err(|error| {
            format!("read timeout setup failed {error:?}; drained_bytes={drained}; first_byte={first_byte:?}")
        })?;
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(drained),
            Ok(read) => {
                first_byte.get_or_insert(buffer[0]);
                drained += read;
                if drained > byte_cap {
                    return Err(format!(
                        "byte cap {byte_cap} exceeded; drained_bytes={drained}; first_byte={first_byte:?}"
                    ));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(drained);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(format!(
                    "transport read failed {error:?}; drained_bytes={drained}; first_byte={first_byte:?}"
                ));
            }
        }
    }
}

fn closure_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let reader = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (writer, _) = listener.accept().unwrap();
    (reader, writer)
}

#[test]
fn closure_oracle_drains_queued_text_before_transport_eof() {
    let (mut reader, mut writer) = closure_pair();
    writer.write_all(b"\x81\x02{}").unwrap();
    writer.shutdown(std::net::Shutdown::Write).unwrap();
    assert_eq!(
        read_transport_close(&mut reader, Instant::now() + Duration::from_secs(3), 32),
        Ok(4)
    );
}

#[test]
fn closure_oracle_rejects_a_silent_live_socket() {
    let (mut reader, writer) = closure_pair();
    assert!(
        read_transport_close(&mut reader, Instant::now() + Duration::from_millis(40), 32).is_err()
    );
    drop(writer);
}

#[test]
fn closure_oracle_rejects_a_close_frame_without_transport_closure() {
    let (mut reader, mut writer) = closure_pair();
    writer.write_all(&[0x88, 0]).unwrap();
    assert!(
        read_transport_close(&mut reader, Instant::now() + Duration::from_millis(40), 32).is_err()
    );
    drop(writer);
}

#[test]
fn closure_oracle_enforces_the_total_byte_cap() {
    let (mut reader, mut writer) = closure_pair();
    writer.write_all(b"\x81\x02{}").unwrap();
    writer.shutdown(std::net::Shutdown::Write).unwrap();
    let error =
        read_transport_close(&mut reader, Instant::now() + Duration::from_secs(3), 3).unwrap_err();
    assert!(error.contains("byte cap 3 exceeded"), "{error}");
}

#[test]
fn selected_reply_arrives_before_child_completion_and_reconnects() {
    let fixture = Fixture::new();
    let clock = Arc::new(TestClock::new());
    let settings = settings(&fixture, free_port());
    let captured = Arc::new(Captured::default());
    let daemon = duded::boot::boot_with_options_and_floor_faults(
        &settings,
        seams(
            &settings,
            ChildEnds::new(),
            Arc::clone(&captured),
            Arc::clone(&clock),
        ),
        duded::settings::RunOptions::default(),
        None,
        FloorStreamFault::default().without_heartbeat(),
    )
    .expect("boot local reply fixture");
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let dir = fixture.state.path().join("REPLY");
    std::fs::create_dir(&dir).unwrap();
    let agent = common::hire(&running.handle, &dir);
    let page = common::floor_session_on(&settings, addr, {
        let clock = Arc::clone(&clock);
        move || clock.now().unix_millis()
    });
    let mut socket = WebSocket::connect(&page);
    socket.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(socket.json()["type"], "floor.snapshot");
    let select = format!(r#"{{"type":"floor.output.select","agent_id":"{agent}","view":"reply"}}"#);
    socket.send(&select);
    assert_eq!(socket.output()["reason"], "no_run_output");
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    wait_file(&dir.join("first-ready"));
    let first = socket.text("REPLY-first");
    assert_eq!(first["type"], "floor.output.reply");
    assert_eq!(first["text"], "REPLY-first");
    assert_eq!(first["ended"], false);
    assert_eq!(first["truncated"], false);
    assert_eq!(first["gap"], Value::Null);
    assert!(!dir.join("release-exit").exists());
    std::fs::write(dir.join("release-next"), b"go").unwrap();
    wait_file(&dir.join("second-ready"));
    captured.wait_for("REPLY-thought");
    let second = socket.text("REPLY-second");
    let text = second["text"].as_str().unwrap();
    assert!(text.starts_with("REPLY-first") && text.ends_with("REPLY-second"));
    assert!(!text.contains("wrong-session") && !text.contains("thought"));
    assert_eq!(second["ended"], false);
    let revision = |frame: &Value| frame["revision"].as_str().unwrap().parse::<u128>().unwrap();
    assert!(revision(&second) > revision(&first));
    let mut reconnect = WebSocket::connect(&page);
    reconnect.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(reconnect.json()["type"], "floor.snapshot");
    reconnect.send(&select);
    let restored = reconnect.output();
    assert_eq!(restored["text"], second["text"]);
    assert_eq!(restored["turn_id"], first["turn_id"]);
    assert!(revision(&restored) >= revision(&second));
    std::fs::write(dir.join("release-exit"), b"go").unwrap();
    let ended = loop {
        let frame = reconnect.output();
        if frame["ended"] == true {
            break frame;
        }
    };
    assert_eq!(ended["text"], second["text"]);
    assert!(revision(&ended) > revision(&second));
    assert_eq!(ended["agent_id"], serde_json::json!(agent));
    drop(socket);
    drop(reconnect);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

fn collect_steps(selected: &mut WebSocket) -> (String, Vec<Value>) {
    let mut seen = String::new();
    let mut steps = Vec::new();
    for _ in 0..16 {
        let frame = selected.output();
        seen.push_str(&frame.to_string());
        if frame["type"] == "floor.output.steps" {
            steps.extend(frame["steps"].as_array().unwrap().clone());
            if steps.len() >= 4 && steps.iter().any(|step| step["status"] == "done") {
                break;
            }
        }
    }
    (seen, steps)
}

/// What a socket that selected nothing hears for a moment: the step kind label and every frame.
fn listen_as_bystander(bystander: &mut WebSocket) -> (Option<String>, String) {
    bystander
        .0
        .set_read_timeout(Some(Duration::from_millis(1_200)))
        .unwrap();
    let mut label = None;
    let mut heard = String::new();
    let until = Instant::now() + Duration::from_millis(1_500);
    while Instant::now() < until
        && let Some(frame) = bystander.try_json()
    {
        heard.push_str(&frame.to_string());
        if frame["type"] == "floor.step.labels" {
            label = frame["labels"][0]["label"].as_str().map(str::to_owned);
        }
        assert!(
            !frame["type"].as_str().unwrap().starts_with("floor.output."),
            "an unselected socket received output: {frame}"
        );
    }
    (label, heard)
}

#[test]
fn steps_reach_only_the_selected_socket_scrubbed_and_others_see_a_kind() {
    let fixture = Fixture::new();
    let clock = Arc::new(TestClock::new());
    let settings = settings(&fixture, free_port());
    let captured = Arc::new(Captured::default());
    let daemon = duded::boot::boot_with_options_and_floor_faults(
        &settings,
        seams(
            &settings,
            ChildEnds::new(),
            Arc::clone(&captured),
            Arc::clone(&clock),
        ),
        duded::settings::RunOptions::default(),
        None,
        FloorStreamFault::default().without_heartbeat(),
    )
    .expect("boot local steps fixture");
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let dir = fixture.state.path().join("STEPS");
    std::fs::create_dir(&dir).unwrap();
    let agent = common::hire(&running.handle, &dir);
    let page = common::floor_session_on(&settings, addr, {
        let clock = Arc::clone(&clock);
        move || clock.now().unix_millis()
    });
    let mut selected = WebSocket::connect(&page);
    selected.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(selected.json()["type"], "floor.snapshot");
    let mut bystander = WebSocket::connect(&page);
    bystander.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(bystander.json()["type"], "floor.snapshot");
    selected.send(&format!(
        r#"{{"type":"floor.output.select","agent_id":"{agent}","view":"reply"}}"#
    ));
    assert_eq!(selected.output()["reason"], "no_run_output");
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    wait_file(&dir.join("steps-ready"));
    captured.wait_for("toolu_plan");
    let (seen, steps) = collect_steps(&mut selected);
    assert!(
        !seen.contains(SECRET_VALUE),
        "the secret reached the selected socket"
    );
    assert!(seen.contains("(redacted)"));
    let by_id = |id: &str| {
        steps
            .iter()
            .rev()
            .find(|step| step["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(by_id("toolu_curl")["status"], "done");
    assert_eq!(by_id("toolu_curl")["result_bytes"], 2);
    assert!(
        by_id("toolu_curl")["args"][0]["value"]
            .as_str()
            .unwrap()
            .contains("Bearer (redacted)")
    );
    assert_eq!(by_id("toolu_aws")["kind"], "read");
    assert!(by_id("toolu_aws")["withheld"].is_string());
    assert_eq!(by_id("toolu_edit")["edit"]["new"], "let key = env();");
    assert_eq!(
        by_id("toolu_edit")["edit"]["old"],
        "let key = \"(redacted)\";"
    );
    assert_eq!(by_id("toolu_plan")["plan"][0]["text"], "Rotate (redacted)");

    let (label, heard) = listen_as_bystander(&mut bystander);
    assert_eq!(label.as_deref(), Some("plan"));
    for private in [SECRET_VALUE, "curl", "toolu_", ".aws", "a.rs", "Rotate"] {
        assert!(
            !heard.contains(private),
            "an unselected socket heard {private}"
        );
    }
    std::fs::write(dir.join("release-exit"), b"go").unwrap();
    drop(selected);
    drop(bystander);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

/// The last frame of each step the page heard, until every id in `ids` is no longer running.
fn collect_by_id(selected: &mut WebSocket, ids: &[&str]) -> (String, Vec<Value>) {
    let mut seen = String::new();
    let mut last: Vec<Value> = Vec::new();
    for _ in 0..64 {
        let frame = selected.output();
        seen.push_str(&frame.to_string());
        if frame["type"] != "floor.output.steps" {
            continue;
        }
        for step in frame["steps"].as_array().unwrap() {
            last.retain(|old| old["id"] != step["id"]);
            last.push(step.clone());
        }
        if ids.iter().all(|id| {
            last.iter()
                .any(|step| step["id"] == *id && step["status"] != "running")
        }) {
            break;
        }
    }
    (seen, last)
}

fn step_of<'a>(steps: &'a [Value], id: &str) -> &'a Value {
    steps
        .iter()
        .find(|step| step["id"] == id)
        .unwrap_or_else(|| panic!("no step {id} in {steps:?}"))
}

#[test]
fn codex_and_pi_steps_reach_the_selected_socket_scrubbed_by_the_same_rules() {
    let fixture = Fixture::new();
    let clock = Arc::new(TestClock::new());
    let settings = settings(&fixture, free_port());
    let captured = Arc::new(Captured::default());
    let daemon = duded::boot::boot_with_options_and_floor_faults(
        &settings,
        seams(
            &settings,
            ChildEnds::new(),
            Arc::clone(&captured),
            Arc::clone(&clock),
        ),
        duded::settings::RunOptions::default(),
        None,
        FloorStreamFault::default().without_heartbeat(),
    )
    .expect("boot local engine steps fixture");
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let page = common::floor_session_on(&settings, addr, {
        let clock = Arc::clone(&clock);
        move || clock.now().unix_millis()
    });
    let mut dirs = Vec::new();
    let mut sockets = Vec::new();
    for label in ["CODEX", "PI"] {
        let dir = fixture.state.path().join(label);
        std::fs::create_dir(&dir).unwrap();
        let agent = common::hire(&running.handle, &dir);
        let mut socket = WebSocket::connect(&page);
        socket.send(r#"{"type":"floor.subscribe"}"#);
        assert_eq!(socket.json()["type"], "floor.snapshot");
        socket.send(&format!(
            r#"{{"type":"floor.output.select","agent_id":"{agent}","view":"reply"}}"#
        ));
        assert_eq!(socket.output()["reason"], "no_run_output");
        common::set_goal(
            &running.handle,
            common::goal_params(agent, &["/usr/bin/true"]),
        );
        dirs.push(dir);
        sockets.push(socket);
    }
    let [codex_dir, pi_dir] = &dirs[..] else {
        unreachable!("two engines");
    };
    wait_file(&codex_dir.join("steps-ready"));
    wait_file(&pi_dir.join("steps-ready"));

    let (seen, steps) = collect_by_id(
        &mut sockets[0],
        &["item_1", "item_2", "item_3", "item_4-1", "item_5"],
    );
    assert!(
        !seen.contains("PRIVATE-AWS-TEXT"),
        "a withheld result reached the socket"
    );
    let ls = step_of(&steps, "item_1");
    assert_eq!(
        (ls["kind"].as_str(), ls["status"].as_str()),
        (Some("command"), Some("done"))
    );
    assert_eq!(ls["args"][0]["value"], "/bin/zsh -lc 'ls'");
    assert_eq!(ls["result_bytes"], 6);
    assert!(step_of(&steps, "item_2")["withheld"].is_string());
    let failed = step_of(&steps, "item_3");
    assert_eq!(
        (failed["status"].as_str(), failed["exit"].as_i64()),
        (Some("failed"), Some(2))
    );
    assert_eq!(step_of(&steps, "item_4-0")["kind"], "write");
    assert_eq!(step_of(&steps, "item_4-1")["kind"], "edit");
    assert_eq!(
        step_of(&steps, "item_4-1")["args"][0]["value"],
        "/repo/a.rs"
    );
    assert_eq!(step_of(&steps, "item_5")["plan"][0]["text"], "Ship it");

    let (seen, steps) = collect_by_id(&mut sockets[1], &["call_1", "call_2", "call_3", "call_4"]);
    assert!(
        !seen.contains("PRIVATE-ENV-TEXT"),
        "a withheld result reached the socket"
    );
    assert_eq!(step_of(&steps, "call_1")["kind"], "command");
    assert_eq!(step_of(&steps, "call_1")["status"], "done");
    assert!(step_of(&steps, "call_2")["withheld"].is_string());
    assert_eq!(step_of(&steps, "call_3")["kind"], "search");
    assert_eq!(step_of(&steps, "call_3")["status"], "failed");
    let edit = step_of(&steps, "call_4");
    assert_eq!(edit["kind"], "edit");
    assert_eq!(edit["edit"]["old"], "let a = 1;");
    assert_eq!(edit["edit"]["new"], "let a = 2;");

    std::fs::write(codex_dir.join("release-exit"), b"go").unwrap();
    std::fs::write(pi_dir.join("release-exit"), b"go").unwrap();
    drop(sockets);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}

fn fetch(socket: &mut WebSocket, agent: &str, turn: &str, step: &str) -> Value {
    socket.send(&format!(
        r#"{{"type":"floor.output.fetch","agent_id":"{agent}","turn_id":"{turn}","step_id":"{step}"}}"#
    ));
    loop {
        let frame = socket.output();
        if frame["type"] == "floor.output.step" && frame["step_id"] == step {
            return frame;
        }
    }
}

fn check_refusals(selected: &mut WebSocket, other_agent: &str, agent: &str, turn: &str) {
    let mut stale = turn.to_owned();
    let last = if stale.ends_with('0') { '1' } else { '0' };
    stale.replace_range(stale.len() - 1.., &last.to_string());
    let refused = [
        fetch(selected, other_agent, turn, "toolu_plain"),
        fetch(selected, agent, &stale, "toolu_plain"),
        fetch(selected, agent, turn, "bad id!"),
    ];
    for frame in &refused {
        assert_eq!(frame["state"], "refused", "{frame}");
        assert_eq!(frame["reason"], "this page has not selected that step");
        assert!(frame.get("result").is_none() && frame.get("edit").is_none());
    }
    let guessed = fetch(selected, agent, turn, "toolu_guess");
    assert_eq!(guessed["state"], "gone", "{guessed}");
    assert!(guessed.get("result").is_none() && guessed.get("reason").is_none());
}

fn check_fetches(selected: &mut WebSocket, agent: &str, turn: &str) {
    let mut heard = String::new();
    let mut ask = |id: &str| {
        let frame = fetch(selected, agent, turn, id);
        heard.push_str(&frame.to_string());
        frame
    };

    let plain = ask("toolu_plain");
    assert_eq!(plain["state"], "ready");
    assert_eq!(plain["result"]["text"], "plain output line");

    let known = ask("toolu_known");
    assert_eq!(known["result"]["text"], "token (redacted) end");

    for (id, private) in [
        ("toolu_shape", "ghp_"),
        ("toolu_env", "PRIVATE-ENV"),
        ("toolu_cat", "PRIVATE-AWS"),
        ("toolu_mcp", "PRIVATE-MCP"),
        ("toolu_bgout", "PRIVATE-BG"),
        ("toolu_grep", "PRIVATE-GREP"),
        ("toolu_mystery", "PRIVATE-OTHER"),
        ("toolu_envfile", "hunter2"),
        ("toolu_numbered", "hunter2"),
        ("toolu_secretedit", "hunter2"),
    ] {
        let frame = ask(id);
        assert_eq!(frame["state"], "withheld", "{id}: {frame}");
        assert!(frame["reason"].is_string(), "{id}: {frame}");
        assert!(frame.get("result").is_none(), "{id}: {frame}");
        assert!(!frame.to_string().contains(private), "{id}: {frame}");
    }

    let big = ask("toolu_big");
    let text = big["result"]["text"].as_str().unwrap();
    assert!(
        text.len() <= 8 * 1024 && big["result"]["cut"] == true,
        "{big}"
    );
    assert!(!text.contains("TAIL-MARK"));

    let edit = ask("toolu_edit");
    assert_eq!(edit["edit"]["new"]["text"].as_str().unwrap().len(), 5_000);
    assert!(edit["edit"]["new"].get("cut").is_none());
    let write = ask("toolu_write");
    assert_eq!(write["edit"]["new"]["cut"], true);
    assert!(write["edit"]["new"]["text"].as_str().unwrap().len() <= 8 * 1024);

    let evicted = ask("toolu_fill0");
    assert_eq!(evicted["state"], "gone");
    assert!(!heard.contains(SECRET_VALUE), "the secret reached the wire");
}

#[test]
fn step_results_are_fetched_scrubbed_and_only_by_the_socket_that_selected_the_agent() {
    let fixture = Fixture::new();
    let clock = Arc::new(TestClock::new());
    let settings = settings(&fixture, free_port());
    let captured = Arc::new(Captured::default());
    let daemon = duded::boot::boot_with_options_and_floor_faults(
        &settings,
        seams(
            &settings,
            ChildEnds::new(),
            Arc::clone(&captured),
            Arc::clone(&clock),
        ),
        duded::settings::RunOptions::default(),
        None,
        FloorStreamFault::default().without_heartbeat(),
    )
    .expect("boot local results fixture");
    let addr = daemon.floor_addr().unwrap();
    let mut running = common::start(daemon);
    let dir = fixture.state.path().join("RESULTS");
    std::fs::create_dir(&dir).unwrap();
    let agent = common::hire(&running.handle, &dir).to_string();
    let other_dir = fixture.state.path().join("OTHER");
    std::fs::create_dir(&other_dir).unwrap();
    let other = common::hire(&running.handle, &other_dir).to_string();
    let page = common::floor_session_on(&settings, addr, {
        let clock = Arc::clone(&clock);
        move || clock.now().unix_millis()
    });
    let mut selected = WebSocket::connect(&page);
    selected.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(selected.json()["type"], "floor.snapshot");
    let mut bystander = WebSocket::connect(&page);
    bystander.send(r#"{"type":"floor.subscribe"}"#);
    assert_eq!(bystander.json()["type"], "floor.snapshot");
    selected.send(&format!(
        r#"{{"type":"floor.output.select","agent_id":"{agent}","view":"reply"}}"#
    ));
    assert_eq!(selected.output()["reason"], "no_run_output");
    common::set_goal(
        &running.handle,
        common::goal_params(agent.parse().unwrap(), &["/usr/bin/true"]),
    );
    wait_file(&dir.join("steps-ready"));
    captured.wait_for("toolu_write");
    let mut turn = None;
    let mut closed = 0;
    for _ in 0..64 {
        let frame = selected.output();
        if frame["type"] == "floor.output.steps" {
            turn = frame["turn_id"].as_str().map(str::to_owned);
            closed += frame["steps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|step| step["status"] == "done" && step["id"] == "toolu_write")
                .count();
        }
        if closed > 0 {
            break;
        }
    }
    let turn = turn.expect("the turn of the steps");
    check_refusals(&mut selected, &other, &agent, &turn);
    check_fetches(&mut selected, &agent, &turn);
    let mut signed_out = TcpStream::connect(page.addr).unwrap();
    signed_out
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    signed_out
        .write_all(
            page.upgrade_head_with("/floor/stream", "dudes-floor-v4", None)
                .as_bytes(),
        )
        .unwrap();
    assert!(!read_head(&mut signed_out).starts_with("HTTP/1.1 101"));
    let refused = fetch(&mut bystander, &agent, &turn, "toolu_plain");
    assert_eq!(refused["state"], "refused");
    assert!(!refused.to_string().contains("plain output line"));
    std::fs::write(dir.join("release-exit"), b"go").unwrap();
    drop(selected);
    drop(bystander);
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}
