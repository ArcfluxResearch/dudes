//! Native saving launches and attached hook notices through host-owned children.

#![cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]

use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use duded::classify::{ExternalChild, PeerBarrier, ProcessRelation, Relation, Resolver};
use duded::ports::{Ask, OpenedPty, PtyChild, PtyCloseOutcome, PtyHostPort, PtyOpenFailure};
use duded::pty_output::OutputSubscription;
use duded::settings::Seams;
use duded::startup::Witness;
use duded::{ChildEnds, ChildTurn, ExitCause, boot};
use dudes_host::{ChildExit, ChildStream, ChildStreams, RealHost};
use dudes_proto::providers::claude::Settings as HookSettings;
use dudes_proto::rpc::agent::AttachParams;
use dudes_proto::rpc::closing::{ClosingParams, ClosingReport, ClosingStarted, ClosingStatus};
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::pty::PtyId;
use dudes_proto::rpc::{Method, Response};
use dudes_proto::{ClosingPhase, ExitStatus, LoopDecision, Record, TurnOutcome};
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_providers::registry::{Adapters, ProvidersConfig};
use dudes_supervisor::host::AdoptionKey;
use dudes_supervisor::testing::FakeRng;
use dudes_types::{AgentId, Lsn, Mode, Provenance, TurnId};
use serde_json::{Value, json};

mod common;

use common::{Fixture, ScriptedRunner, TestClock};

const ROLE: &str = "DUDES_CLOSING_NATIVE_ROLE";
const SESSION: &str = "DUDES_CLOSING_NATIVE_SESSION";
const SETTINGS: &str = "DUDES_CLOSING_NATIVE_SETTINGS";
const WAIT: Duration = Duration::from_secs(20);

fn wait_for(path: &Path) {
    let deadline = Instant::now() + WAIT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "missing owned marker {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn publish(path: &Path, value: &Value) {
    let partial = path.with_extension("partial");
    std::fs::write(&partial, serde_json::to_vec(value).unwrap()).expect("write observation");
    std::fs::rename(partial, path).expect("publish observation");
}

fn observation(dir: &Path, name: &str) -> Value {
    let path = dir.join(name);
    wait_for(&path);
    serde_json::from_slice(&std::fs::read(path).unwrap()).expect("observation JSON")
}

fn release(dir: &Path, name: &str) {
    std::fs::write(dir.join(name), b"").expect("release owned helper");
}

fn binary(name: &str) -> PathBuf {
    let path = std::env::current_exe()
        .expect("test executable")
        .parent()
        .and_then(Path::parent)
        .expect("target profile directory")
        .join(name);
    assert!(
        path.is_file(),
        "{name} is missing at {}; a targeted `-p duded` run builds only this package's own \
         binaries, so run `cargo build --locked -p dudes -p dudes-hook` first (a full workspace \
         run builds every binary already)",
        path.display()
    );
    path
}

struct OwnedChild(Option<Child>);

impl OwnedChild {
    fn output(mut self) -> Output {
        let deadline = Instant::now() + WAIT;
        while self
            .0
            .as_mut()
            .unwrap()
            .try_wait()
            .expect("poll owned child")
            .is_none()
        {
            assert!(
                Instant::now() < deadline,
                "owned descendant exceeded its deadline"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        self.0
            .take()
            .unwrap()
            .wait_with_output()
            .expect("collect owned child")
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[allow(
    clippy::disallowed_types,
    reason = "native test descendants run the built CLI and installed hook beneath the host-owned stand-in"
)]
fn own_ack(lsn: Lsn) -> Value {
    let child = std::process::Command::new(binary("dudes"))
        .args([
            "--json",
            "closing",
            "ack",
            "--announcement-lsn",
            &lsn.0.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn own acknowledgement CLI");
    let output = OwnedChild(Some(child)).output();
    assert!(
        output.status.success(),
        "own acknowledgement CLI failed without exposing its environment"
    );
    assert!(
        output.stderr.is_empty(),
        "own acknowledgement CLI has no diagnostic"
    );
    serde_json::from_slice(&output.stdout).expect("own acknowledgement JSON")
}

#[allow(
    clippy::disallowed_types,
    reason = "the native attached stand-in executes its exact installed hook as an owned descendant"
)]
fn installed_hook(settings: &Path) -> Value {
    let installed: HookSettings =
        serde_json::from_slice(&std::fs::read(settings).expect("installed hook settings")).unwrap();
    let handler = common::hook_shim::handler(&installed, "UserPromptSubmit");
    let child = std::process::Command::new(handler.command.as_deref().unwrap())
        .args(handler.args.as_deref().unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn installed hook");
    let mut owned = OwnedChild(Some(child));
    let pid = owned.0.as_ref().unwrap().id();
    owned
        .0
        .as_mut()
        .unwrap()
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"hook_event_name":"UserPromptSubmit","prompt":"native boundary"}"#)
        .expect("one hook payload and close");
    let output = owned.output();
    assert!(output.status.success(), "admitted prompt hook succeeds");
    assert!(output.stderr.is_empty(), "hook has no diagnostic");
    json!({"pid":pid,"reply":serde_json::from_slice::<Value>(&output.stdout).unwrap_or(Value::Null)})
}

fn announced_lsn(task: &str) -> Lsn {
    let tail = task
        .split("dudes closing ack --announcement-lsn ")
        .nth(1)
        .expect("received own acknowledgement instruction");
    Lsn(tail
        .split('`')
        .next()
        .unwrap()
        .parse()
        .expect("announcement position"))
}

fn completed(session: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", json!({"type":"result","subtype":"success","is_error":false,"session_id":session,"result":"native stand-in completed","total_cost_usd":0.0,"modelUsage":{}})).unwrap();
    out.flush().unwrap();
}

#[test]
fn the_helper_entry_point() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let dir = std::env::current_dir().unwrap();
    if role == "attached" {
        publish(
            &dir.join("attached-ready.json"),
            &json!({"pid":std::process::id()}),
        );
        wait_for(&dir.join("hook.release"));
        let hook = installed_hook(&PathBuf::from(std::env::var_os(SETTINGS).unwrap()));
        publish(&dir.join("hook.json"), &hook);
        let context = hook["reply"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("actual hook delivered closing context");
        let lsn = announced_lsn(context);
        publish(
            &dir.join("attached-saved.json"),
            &json!({"pid":std::process::id(),"context":context}),
        );
        let ack = own_ack(lsn);
        assert_eq!(own_ack(lsn), ack);
        publish(
            &dir.join("attached-ack.json"),
            &json!({"pid":std::process::id(),"result":ack}),
        );
        wait_for(&dir.join("attached.release"));
    } else {
        let mut task = String::new();
        std::io::stdin()
            .read_to_string(&mut task)
            .expect("launch task then EOF");
        let session = std::env::var(SESSION).unwrap();
        let mut out = std::io::stdout().lock();
        writeln!(
            out,
            "{}",
            json!({"type":"system","subtype":"init","session_id":session,"tools":[]})
        )
        .unwrap();
        out.flush().unwrap();
        drop(out);
        if task.contains("Closing time.") {
            let lsn = announced_lsn(&task);
            publish(
                &dir.join("saved.json"),
                &json!({"pid":std::process::id(),"task":task}),
            );
            let ack = own_ack(lsn);
            assert_eq!(own_ack(lsn), ack);
            publish(
                &dir.join("ack.json"),
                &json!({"pid":std::process::id(),"result":ack}),
            );
            wait_for(&dir.join("saving.release"));
        } else {
            publish(
                &dir.join("original.json"),
                &json!({"pid":std::process::id(),"task":task}),
            );
            wait_for(&dir.join("original.release"));
        }
        completed(&session);
    }
    std::process::exit(0);
}

#[derive(Debug)]
struct HostProcesses(dudes_host::PeerProcesses);

impl ProcessRelation for HostProcesses {
    fn begin_spawn(&self) -> u64 {
        self.0.begin_spawn()
    }
    fn finish_spawn(&self, slot: u64, key: Option<&AdoptionKey>) -> bool {
        self.0.finish_spawn(slot, key)
    }
    fn published(&self, children: &[AdoptionKey]) -> Vec<(u64, usize)> {
        self.0.published(children)
    }
    fn resolve(&self, peer: u32, pidversion: Option<u32>, children: &[AdoptionKey]) -> Relation {
        #[cfg(target_os = "macos")]
        let found = pidversion.map_or(dudes_host::PeerRelation::Unresolvable, |version| {
            self.0.resolve_execution(peer, version, children)
        });
        #[cfg(target_os = "linux")]
        let found = {
            let _ = pidversion;
            self.0.resolve(peer, children)
        };
        match found {
            dudes_host::PeerRelation::Matched(index) => Relation::Matched(index),
            dudes_host::PeerRelation::Pending(slot) => Relation::Pending(slot),
            dudes_host::PeerRelation::NoMatch => Relation::NoMatch,
            dudes_host::PeerRelation::Unresolvable => Relation::Unresolvable,
            dudes_host::PeerRelation::Abandoned => Relation::Abandoned,
        }
    }
    fn track_external(&self, slot: u64) -> Box<dyn ExternalChild> {
        Box::new(HostExternal(self.0.track_external(slot)))
    }
}

#[derive(Debug)]
struct HostExternal(dudes_host::ExternalChild);
impl ExternalChild for HostExternal {
    fn register(&mut self, child: PtyChild) -> std::io::Result<AdoptionKey> {
        self.0.register(child.pid)
    }
}

struct HostStreams(Arc<ChildEnds>);
impl ChildStreams for HostStreams {
    fn watch(&self, key: Option<&AdoptionKey>) -> Box<dyn ChildStream> {
        Box::new(HostTurn(self.0.watch(key)))
    }
}
struct HostTurn(ChildTurn);
impl ChildStream for HostTurn {
    fn line(&mut self, text: &str) {
        self.0.line(text);
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

#[derive(Clone, Default)]
struct NativePtys {
    host: Arc<Mutex<dudes_pty::PtyHost>>,
    readers: Arc<Mutex<Vec<dudes_pty::PtyReader>>>,
}
impl PtyHostPort for NativePtys {
    fn open(
        &mut self,
        spec: LaunchSpec,
        cols: u16,
        rows: u16,
    ) -> Result<OpenedPty, PtyOpenFailure> {
        let mut host = self.host.lock().unwrap();
        match host.open(spec, dudes_pty::PtySize { cols, rows }) {
            Ok(opened) => {
                self.readers.lock().unwrap().push(opened.reader);
                let pid = host.pid(opened.id).unwrap();
                Ok(OpenedPty {
                    pty: opened.id,
                    child: PtyChild { pid },
                })
            }
            Err(dudes_pty::OpenError::Failed(error)) => Err(PtyOpenFailure::Failed(error)),
            Err(dudes_pty::OpenError::CleanupUnconfirmed {
                pty,
                cause,
                cleanup,
            }) => Err(PtyOpenFailure::CleanupUnconfirmed {
                pty,
                cause,
                cleanup,
            }),
        }
    }
    fn write(&mut self, _pty: PtyId, _bytes: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other(
            "native closing test admits no PTY input",
        ))
    }
    fn resize(&mut self, pty: PtyId, cols: u16, rows: u16) -> std::io::Result<()> {
        self.host
            .lock()
            .unwrap()
            .resize(pty, dudes_pty::PtySize { cols, rows })
    }
    fn subscribe(&mut self, _pty: PtyId) -> Option<OutputSubscription> {
        None
    }
    fn close(&mut self, pty: PtyId) -> PtyCloseOutcome {
        match self.host.lock().unwrap().close(pty) {
            Ok(dudes_pty::CloseOutcome::Closed) => PtyCloseOutcome::Closed,
            Ok(dudes_pty::CloseOutcome::AlreadyClosed) => PtyCloseOutcome::AlreadyClosed,
            Err(error) => PtyCloseOutcome::Failed(error),
        }
    }
    fn shutdown(&mut self) -> std::io::Result<()> {
        self.host.lock().unwrap().shutdown()?;
        self.readers.lock().unwrap().clear();
        Ok(())
    }
}

struct Releases(Vec<PathBuf>);
impl Drop for Releases {
    fn drop(&mut self) {
        for dir in &self.0 {
            for name in [
                "original.release",
                "saving.release",
                "hook.release",
                "attached.release",
            ] {
                let _ = std::fs::write(dir.join(name), b"");
            }
        }
    }
}

fn setup(fixture: &Fixture, grace: Duration) -> (duded::Settings, common::Running, NativePtys) {
    let helper = std::env::current_exe().unwrap();
    let stand_in = fixture.state.path().join("native-engine");
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = auth ]; then exit 0; fi
if [ "$1" = --resume ] && [ "$3" = --settings ]; then
  export {ROLE}=attached
  export {SETTINGS}="$4"
else
  export {ROLE}=managed
  while [ "$#" -gt 0 ]; do
    case "$1" in --session-id|--resume) export {SESSION}="$2"; shift;; esac
    shift
  done
fi
exec '{}' --exact the_helper_entry_point --quiet --nocapture
"#,
        helper.display()
    );
    std::fs::write(&stand_in, script).unwrap();
    std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = fixture.state.path().join("scratch-home");
    std::fs::create_dir(&home).unwrap();
    let settings = duded::Settings {
        daemon_env: vec![
            EnvVar::new("PATH", "/usr/bin:/bin"),
            EnvVar::new("HOME", home.as_os_str()),
        ],
        hook_command: Some(binary("dudes-hook")),
        ..fixture.settings()
    };
    let adapters = Adapters::from_config(
        &ProvidersConfig {
            claude: Some(ClaudeConfig {
                command: stand_in.into_os_string(),
                task_argv_max_bytes: 1,
                ..ClaudeConfig::default()
            }),
        },
        &settings.daemon_env,
    )
    .unwrap();
    let children = ChildEnds::new();
    let host = RealHost::watching(Arc::new(HostStreams(Arc::clone(&children))));
    let resolver = Resolver::processes(Arc::new(HostProcesses(host.peer_processes())));
    let ptys = NativePtys::default();
    let seams = Seams {
        host: PeerBarrier::new(host, resolver.clone()),
        pty_host: Box::new(ptys.clone()),
        predicates: Arc::new(ScriptedRunner::scripted([common::fails()])),
        clock: Arc::new(TestClock::new()),
        rng: common::FixtureRng::from_rng(&settings, FakeRng::new()),
        resolver,
        adapters,
        children,
        witness: Witness::default(),
    };
    let mut daemon = boot(&settings, seams).expect("boot real host and shipped resolver");
    daemon.closing_grace_for_test(grace);
    (settings, common::start(daemon), ptys)
}

fn operator(settings: &duded::Settings) -> OperatorCredential {
    OperatorCredential::new(
        std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .unwrap(),
    )
    .unwrap()
}
fn call(
    client: &mut dudes_rpc::LocalClient,
    settings: &duded::Settings,
    params: &ClosingParams,
) -> Response {
    client
        .call_as_operator(
            Method::SystemClosingTime,
            Some(serde_json::to_value(params).unwrap()),
            operator(settings),
        )
        .unwrap()
}
fn announce(client: &mut dudes_rpc::LocalClient, settings: &duded::Settings) -> ClosingStarted {
    let reply = call(client, settings, &ClosingParams::Announce);
    assert!(
        reply.error.is_none(),
        "announcement refused: {:?}",
        reply.error
    );
    serde_json::from_value(reply.result.unwrap()).unwrap()
}
/// Attach, retrying while the daemon's own fold of the just-observed `TurnEnded` is still in
/// flight behind this RPC call.
///
/// The journal write this test waited on (`ended`) and the supervisor's in-memory fold of that
/// same record travel on separate paths: the write lands on disk as soon as it is fsynced, but
/// `open_turn` only clears once the decide loop processes the matching `Input::Persisted`. An
/// isolated test can win that race and call attach before the fold catches up; a loaded run
/// adds enough latency elsewhere that it usually does not. Retrying the exact transient refusal
/// closes the gap without weakening what the test asserts once it stops retrying.
fn attach(
    client: &mut dudes_rpc::LocalClient,
    settings: &duded::Settings,
    params: &AttachParams,
) -> Response {
    let deadline = Instant::now() + WAIT;
    loop {
        let reply = client
            .call_as_operator(
                Method::AgentAttach,
                Some(serde_json::to_value(params).unwrap()),
                operator(settings),
            )
            .unwrap();
        let transient = reply.error.as_ref().is_some_and(|error| {
            error.message == "that agent cannot be attached in its current state"
        });
        if !transient {
            return reply;
        }
        assert!(
            Instant::now() < deadline,
            "native attach stayed refused past the observed turn end"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn status(
    client: &mut dudes_rpc::LocalClient,
    settings: &duded::Settings,
    lsn: Lsn,
) -> ClosingStatus {
    let reply = call(
        client,
        settings,
        &ClosingParams::Status { lsn, after: None },
    );
    assert!(reply.error.is_none(), "status refused: {:?}", reply.error);
    serde_json::from_value(reply.result.unwrap()).unwrap()
}
fn pid(observed: &Value) -> u32 {
    u32::try_from(observed["pid"].as_u64().unwrap()).unwrap()
}
fn ended(fixture: &Fixture, turn: TurnId) -> Lsn {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(lsn) = fixture.entries().iter().find_map(|(lsn, entry)| {
            matches!(
                entry.record,
                Record::TurnEnded {
                    turn: who,
                    mode: Mode::Managed,
                    provenance: Provenance::Observed,
                    outcome: Some(TurnOutcome::Completed),
                    exit: ExitStatus { code: Some(0), signal: None },
                    ..
                } if who == turn
            )
            .then_some(*lsn)
        }) {
            return lsn;
        }
        assert!(
            Instant::now() < deadline,
            "no separate normal Observed Managed end"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn complete(fixture: &Fixture) -> Lsn {
    fixture.await_record("Closing Complete", |record| {
        matches!(
            record,
            Record::ClosingTime {
                phase: ClosingPhase::Complete
            }
        )
        .then_some(())
    });
    fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| {
            matches!(
                entry.record,
                Record::ClosingTime {
                    phase: ClosingPhase::Complete
                }
            )
            .then_some(lsn)
        })
        .unwrap()
}
fn hire_at(running: &common::Running, dir: &Path) -> AgentId {
    std::fs::create_dir(dir).unwrap();
    let agent = common::hire(&running.handle, dir);
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/false"]),
    );
    agent
}

fn sample(
    kind: &str,
    announced: Instant,
    ack: Option<Duration>,
    exited: Instant,
    children: &[u32],
) {
    let alive_at_return = children.iter().any(|pid| !common::finished(*pid));
    for pid in children {
        common::await_finished(*pid, "measured closing participant");
    }
    if let Some(directory) = std::env::var_os("DUDES_CLOSING_TIMING_DIRECTORY") {
        let observation = json!({
            "scenario":kind,
            "announcement_to_ack_ms":ack.map(|elapsed| elapsed.as_secs_f64() * 1_000.0),
            "closing_ms":exited.duration_since(announced).as_secs_f64() * 1_000.0,
            "alive_at_daemon_return":alive_at_return,
            "child_lifetime_after_return_ms":if alive_at_return { exited.elapsed().as_secs_f64() * 1_000.0 } else { 0.0 },
        });
        std::fs::write(
            Path::new(&directory).join(format!("{kind}.json")),
            serde_json::to_vec(&observation).unwrap(),
        )
        .unwrap();
    }
}

fn assert_saving_task(fixture: &Fixture, saving: TurnId, agent: AgentId, received: &Value) -> Lsn {
    let (issued, decision_lsn, task, answers) = fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| match entry.record {
            Record::TurnIssued {
                turn,
                task,
                lsn_of_decision,
                answers,
                ..
            } if turn == saving => Some((lsn, lsn_of_decision, task, answers)),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        received["task"], task,
        "launch stdin carries actual journaled saving task"
    );
    assert!(answers.is_empty());
    let decision = fixture
        .entries()
        .into_iter()
        .find(|(lsn, _)| *lsn == decision_lsn)
        .unwrap()
        .1;
    assert!(matches!(decision.record, Record::LoopDecided {
        agent: owner, decision: LoopDecision::Continue { task: decided }, input_digest, ..
    } if owner == agent && decided == task && input_digest.len() == 64));
    assert!(
        decision_lsn < issued,
        "the exact saving decision precedes its issue"
    );
    issued
}

#[test]
fn managed_saving_task_acknowledges_alive_and_completes_only_after_observed_exit() {
    let fixture = Fixture::new();
    let (settings, mut running, _ptys) = setup(&fixture, Duration::from_secs(15));
    let dir = fixture.state.path().join("managed");
    let _releases = Releases(vec![dir.clone()]);
    let agent = hire_at(&running, &dir);
    let original = observation(&dir, "original.json");
    let original_turn = fixture.await_turn();
    assert!(!common::finished(pid(&original)));
    let mut client = dudes_rpc::Client::connect(settings.socket()).unwrap();
    let announced_at = Instant::now();
    let started = announce(&mut client, &settings);
    assert_eq!(started.participants, vec![agent]);
    release(&dir, "original.release");
    let original_end = ended(&fixture, original_turn);
    let saved = observation(&dir, "saved.json");
    let acknowledged = observation(&dir, "ack.json");
    let ack_elapsed = announced_at.elapsed();
    assert_eq!(acknowledged["result"], json!({}));
    assert!(
        !common::finished(pid(&saved)),
        "saving process remains alive after own ACK"
    );
    let progress = status(&mut client, &settings, started.announcement);
    assert_eq!((progress.acknowledged, progress.total), (1, 1));
    assert_eq!(progress.phase, ClosingPhase::Ack { agent });
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record,
                Record::ClosingTime { phase: ClosingPhase::Ack { agent: owner } } if *owner == agent
            ))
            .count(),
        1
    );
    let saving = progress.participants[0]
        .saving_turn
        .expect("actual saving turn");
    let issued = assert_saving_task(&fixture, saving, agent, &saved);
    assert!(original_end < issued);
    assert!(fixture.records().iter().any(|record| matches!(
        record,
        Record::PredicateResult {
            pass: false,
            provenance: Provenance::Observed,
            ..
        }
    )));
    assert!(!fixture.records().iter().any(|record| matches!(
        record,
        Record::ClosingTime {
            phase: ClosingPhase::Complete
        } | Record::LoopDecided {
            decision: LoopDecision::Done,
            ..
        }
    )));
    release(&dir, "saving.release");
    let saving_end = ended(&fixture, saving);
    let completion = complete(&fixture);
    assert!(saving_end < completion);
    drop(client);
    assert_eq!(running.join(), ExitCause::ShutdownRpc);
    sample(
        "cooperative",
        announced_at,
        Some(ack_elapsed),
        Instant::now(),
        &[pid(&original), pid(&saved)],
    );
    common::await_finished(pid(&original), "original managed helper");
    common::await_finished(pid(&saved), "saving managed helper");
}

#[test]
fn attached_descendant_receives_next_hook_notice_and_acks_without_pty_task_input() {
    let fixture = Fixture::new();
    let (settings, mut running, _ptys) = setup(&fixture, Duration::from_secs(15));
    let attached = fixture.state.path().join("attached");
    let held = fixture.state.path().join("held-managed");
    let _releases = Releases(vec![attached.clone(), held.clone()]);
    let mut client = dudes_rpc::Client::connect(settings.socket()).unwrap();
    let ping = client.call(Method::SystemPing, None).unwrap();
    assert!(ping.error.is_none());
    let agent = hire_at(&running, &attached);
    let original = observation(&attached, "original.json");
    let original_turn = fixture.await_turn();
    running.handle.ask(Ask::Pause(agent)).unwrap();
    release(&attached, "original.release");
    ended(&fixture, original_turn);
    let params = AttachParams {
        agent,
        cols: 80,
        rows: 24,
    };
    let reply = attach(&mut client, &settings, &params);
    assert!(
        reply.error.is_none(),
        "native attach refused: {:?}",
        reply.error
    );
    let ready = observation(&attached, "attached-ready.json");
    running.handle.ask(Ask::Resume(agent)).unwrap();
    let managed = hire_at(&running, &held);
    let managed_original = observation(&held, "original.json");
    let started = announce(&mut client, &settings);
    let mut participants = vec![agent, managed];
    participants.sort();
    assert_eq!(started.participants, participants);
    release(&attached, "hook.release");
    let hook = observation(&attached, "hook.json");
    let context = hook["reply"]["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert_eq!(announced_lsn(context), started.announcement);
    assert!(context.contains("Save or park current work"));
    observation(&attached, "attached-saved.json");
    let ack = observation(&attached, "attached-ack.json");
    assert_eq!(ack["result"], json!({}));
    assert!(
        !common::finished(pid(&ready)),
        "attached helper is alive after own ACK"
    );
    let progress = status(&mut client, &settings, started.announcement);
    assert_eq!((progress.acknowledged, progress.total), (1, 2));
    assert!(
        progress
            .participants
            .iter()
            .find(|row| row.agent == agent)
            .unwrap()
            .acknowledged
    );
    assert!(!fixture.records().iter().any(|record| matches!(
        record,
        Record::PtyWriteRequested { .. }
            | Record::PtyWriteCompleted { .. }
            | Record::ClosingTime {
                phase: ClosingPhase::Complete
            }
    )));
    release(&attached, "attached.release");
    release(&held, "original.release");
    let managed_saved = observation(&held, "saved.json");
    let managed_ack = observation(&held, "ack.json");
    assert_eq!(managed_ack["result"], json!({}));
    let progress = status(&mut client, &settings, started.announcement);
    assert_eq!((progress.acknowledged, progress.total), (2, 2));
    let saving = progress
        .participants
        .iter()
        .find(|row| row.agent == managed)
        .unwrap()
        .saving_turn
        .unwrap();
    assert!(!common::finished(pid(&managed_saved)));
    release(&held, "saving.release");
    let saving_end = ended(&fixture, saving);
    assert!(saving_end < complete(&fixture));
    drop(client);
    assert_eq!(running.join(), ExitCause::ShutdownRpc);
    for (observed, label) in [
        (&original, "attached session acquisition"),
        (&ready, "attached PTY helper"),
        (&managed_original, "managed original"),
        (&managed_saved, "managed saving"),
    ] {
        common::await_finished(pid(observed), label);
    }
    common::await_finished(pid(&hook), "installed hook descendant");
}

#[test]
fn an_ignoring_native_child_is_missing_at_the_bound_and_is_reaped() {
    let fixture = Fixture::new();
    let grace = Duration::from_millis(300);
    let (settings, mut running, _ptys) = setup(&fixture, grace);
    let dir = fixture.state.path().join("ignoring");
    let _releases = Releases(vec![dir.clone()]);
    let agent = hire_at(&running, &dir);
    let original = observation(&dir, "original.json");
    let mut client = dudes_rpc::Client::connect(settings.socket()).unwrap();
    let called_at = Instant::now();
    let started = announce(&mut client, &settings);
    assert_eq!(started.participants, vec![agent]);
    let progress = status(&mut client, &settings, started.announcement);
    assert_eq!((progress.acknowledged, progress.total), (0, 1));
    assert_eq!(progress.phase, ClosingPhase::Announced);
    assert!(
        !common::finished(pid(&original)),
        "ignoring child is still alive before the bound"
    );
    let completion = complete(&fixture);
    assert!(
        called_at.elapsed() >= grace,
        "completion waits for the owned fixture bound"
    );
    drop(client);
    assert_eq!(running.join(), ExitCause::ShutdownRpc);
    assert!(called_at.elapsed() < WAIT, "bounded teardown completes");
    sample(
        "ignoring",
        called_at,
        None,
        Instant::now(),
        &[pid(&original)],
    );
    let report: ClosingReport =
        serde_json::from_slice(&std::fs::read(started.report.path).unwrap()).unwrap();
    assert_eq!(report.announcement, started.announcement);
    assert_eq!(report.complete, completion);
    assert_eq!(report.participants, vec![agent]);
    assert_eq!(report.acknowledged, Vec::<AgentId>::new());
    assert_eq!(report.missing, vec![agent]);
    common::await_finished(pid(&original), "ignoring managed child");
}
