//! Signing in to an engine from the Floor.
//!
//! One fixed command per engine runs in a daemon-owned terminal, and the page draws it. The page
//! picks only the engine. The command, its program, its environment and its directory are the
//! daemon's, built by a [`Launcher`] from what the provider check found.
//!
//! The terminal's output goes to the page and nowhere else: it is not journaled, not logged and
//! not kept after the sign-in ends. What the person types goes to the terminal and nowhere else.
//! The daemon never types into it. The engine writes its own credentials; the daemon reads none.
//!
//! A sign-in ends, and its whole process tree with it, when the page closes it, when the page's
//! socket goes, after the time limit, when the daemon stops, and when a second sign-in for the same
//! engine starts. There is at most one sign-in per engine.

use std::collections::BTreeMap;
use std::io::{self, Read as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dudes_proto::rpc::pty::PtyId;
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_rpc::methods::pty::decode_terminal_input;
use dudes_types::Provider;
use serde::{Deserialize, Serialize};

use crate::floor_gateway::{ACCEPT_POLL, stop_requested, write_bytes};
use crate::floor_session::LoginSession;
use crate::floor_switches::SharedSwitches;
use crate::floor_websocket::{
    Decoder, Message, base64, close_frame, policy_close_frame, pong_frame, text_frame,
};
use crate::ports::{ClosedPtyHost, PtyCloseOutcome, PtyHostPort};
use crate::provider_check::ProviderCheck;
use crate::pty_output::{Output, OutputSubscription};
use crate::runtime::terminal_size_in_bounds;
use crate::settings::{ENGINE_LOGIN_TIMEOUT, EngineLogin};
use crate::sign_in::SignInCheck;

pub(crate) const PROTOCOL: &str = "dudes-engine-login-v1";

/// How long the daemon waits for the engine to report an exit after its output closes.
const EXIT_WAIT: Duration = Duration::from_secs(2);

/// The most one message of the person's typing carries. A paste is split by the page.
const MAX_INPUT_BYTES: usize = 4_096;

/// The most the person may type into one sign-in, which no login takes more than a code of.
const MAX_TOTAL_INPUT_BYTES: usize = 64 * 1024;

/// How long the daemon offers the person's typing to a terminal that has no room.
const INPUT_WAIT: Duration = Duration::from_millis(200);

/// A silent socket's session and switch are rechecked this often.
const RECHECK_AFTER: Duration = Duration::from_millis(1_500);

/// An engine the Floor can sign in to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Engine {
    Claude,
    Codex,
    Pi,
}

impl Engine {
    const fn provider(self) -> Provider {
        match self {
            Self::Claude => Provider::Claude,
            Self::Codex => Provider::Codex,
            Self::Pi => Provider::Pi,
        }
    }

    /// The arguments after the program. Pi has none: its login is a command typed inside it.
    const fn args(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["auth", "login"],
            Self::Codex => &["login"],
            Self::Pi => &[],
        }
    }
}

/// How a sign-in ended, as the page is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// The engine's login exited 0.
    Succeeded,
    /// The engine's login exited another way, or its output outran the page.
    Failed,
    /// The time limit passed.
    TimedOut,
    /// The page closed it.
    Closed,
    /// A newer sign-in for the same engine took its place.
    Replaced,
}

/// Why a sign-in did not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Refusal {
    /// The daemon did not find the engine's program.
    NotInstalled,
    /// The engine's sign-in cannot be prepared on this machine.
    Unavailable,
    /// The terminal could not be opened.
    CouldNotStart,
    /// Pi opens a full agent session, so signing in to it needs the Floor's terminal switched on.
    NeedsTerminal,
}

type Launch = dyn Fn(Engine, &Path) -> Result<LaunchSpec, Refusal> + Send + Sync;

/// The daemon's sign-ins. Clones share them.
#[derive(Clone)]
pub(crate) struct EngineLogins(Arc<Shared>);

impl std::fmt::Debug for EngineLogins {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EngineLogins")
            .finish_non_exhaustive()
    }
}

struct Shared {
    launch: Box<Launch>,
    /// Told when an engine's sign-in ends, since its credentials may have changed.
    ended: Box<dyn Fn(Provider) + Send + Sync>,
    timeout: Duration,
    host: Mutex<Box<dyn PtyHostPort>>,
    live: Mutex<BTreeMap<Engine, Arc<Login>>>,
    next: AtomicU64,
}

impl Shared {
    /// End every sign-in's process tree.
    fn stop(&self) {
        if let Err(error) = locked(&self.host).shutdown() {
            tracing::warn!(%error, "engine sign-in terminals were not all confirmed closed at stop");
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One running sign-in: the engine, its terminal and its deadline.
struct Login {
    id: u64,
    engine: Engine,
    pty: PtyId,
    deadline: Instant,
    /// The empty directory it runs in.
    scratch: PathBuf,
    /// The first reason anyone gave for ending it. The first reason wins.
    stop: Mutex<Option<Outcome>>,
    /// Whether the host confirmed its whole tree gone.
    closed: AtomicBool,
}

fn locked<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl EngineLogins {
    pub(crate) fn new(
        launch: impl Fn(Engine, &Path) -> Result<LaunchSpec, Refusal> + Send + Sync + 'static,
        ended: impl Fn(Provider) + Send + Sync + 'static,
        host: Box<dyn PtyHostPort>,
        timeout: Duration,
    ) -> Self {
        Self(Arc::new(Shared {
            launch: Box::new(launch),
            ended: Box::new(ended),
            timeout,
            host: Mutex::new(host),
            live: Mutex::default(),
            next: AtomicU64::new(1),
        }))
    }

    /// The sign-ins the daemon runs: the provider check's program for each engine, in a minimal
    /// environment and an empty directory, in a terminal host of their own.
    pub(crate) fn for_daemon(
        check: ProviderCheck,
        envs: BTreeMap<Provider, Vec<EnvVar>>,
        sign_in: SignInCheck,
        login: Option<EngineLogin>,
    ) -> Self {
        let (host, timeout) = match login {
            Some(login) => ((login.host)(), login.timeout),
            None => (
                Box::new(ClosedPtyHost) as Box<dyn PtyHostPort>,
                ENGINE_LOGIN_TIMEOUT,
            ),
        };
        Self::new(
            move |engine, scratch| launch_spec(&check, &envs, scratch, engine),
            move |provider| sign_in.forget(provider),
            host,
            timeout,
        )
    }

    /// Start `engine`'s sign-in. A live one for the same engine ends only after this one opened.
    /// The returned handle ends the sign-in when it drops.
    fn start(&self, engine: Engine, cols: u16, rows: u16) -> Result<Running, Refusal> {
        let shared = &self.0;
        let id = shared.next.fetch_add(1, Ordering::Relaxed);
        let scratch = scratch_dir().map_err(|_| Refusal::CouldNotStart)?;
        let started = shared.open(engine, &scratch, cols, rows);
        let (pty, output) = match started {
            Ok(opened) => opened,
            Err(refusal) => {
                let _ = std::fs::remove_dir_all(&scratch);
                return Err(refusal);
            }
        };
        let login = Arc::new(Login {
            id,
            engine,
            pty,
            deadline: Instant::now() + shared.timeout,
            scratch,
            stop: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let earlier = locked(&shared.live).insert(engine, Arc::clone(&login));
        if let Some(earlier) = earlier {
            shared.end(&earlier, Outcome::Replaced);
        }
        Ok(Running {
            shared: Arc::clone(shared),
            login,
            output,
            outcome: None,
            natural: None,
            typed: 0,
        })
    }
}

/// An empty directory only the daemon's user can enter, for a sign-in to start in.
fn scratch_dir() -> io::Result<PathBuf> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let path =
        std::env::temp_dir().join(format!("dudes-sign-in-{:016x}", u64::from_le_bytes(random)));
    #[cfg(unix)]
    let builder = {
        let mut builder = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = std::fs::DirBuilder::new();
    builder.create(&path)?;
    Ok(path)
}

impl Shared {
    fn open(
        &self,
        engine: Engine,
        scratch: &Path,
        cols: u16,
        rows: u16,
    ) -> Result<(PtyId, OutputSubscription), Refusal> {
        let spec = (self.launch)(engine, scratch)?;
        let mut host = locked(&self.host);
        let opened = host
            .open(spec, cols, rows)
            .map_err(|_| Refusal::CouldNotStart)?;
        let Some(output) = host.subscribe(opened.pty) else {
            let _ = host.close(opened.pty);
            return Err(Refusal::CouldNotStart);
        };
        Ok((opened.pty, output))
    }

    /// Record why `login` is ending, then stop and reap its whole tree before returning. Whether
    /// the host confirmed the tree gone is answered, and logged when it is not.
    fn end(&self, login: &Login, reason: Outcome) -> bool {
        locked(&login.stop).get_or_insert(reason);
        self.close(login)
    }

    fn close(&self, login: &Login) -> bool {
        let confirmed = match locked(&self.host).close(login.pty) {
            PtyCloseOutcome::Failed(error) => {
                tracing::warn!(engine = ?login.engine, %error, "an engine sign-in's process tree is not confirmed closed");
                false
            }
            PtyCloseOutcome::Closed | PtyCloseOutcome::AlreadyClosed => true,
        };
        if confirmed {
            login.closed.store(true, Ordering::Release);
        }
        confirmed
    }

    /// How the engine ended by itself: exit 0 is success, and anything else, including no answer in
    /// time, is a failure.
    fn exit_outcome(&self, login: &Login) -> Outcome {
        let give_up = Instant::now() + EXIT_WAIT;
        loop {
            match locked(&self.host).exit_success(login.pty) {
                Ok(Some(true)) => return Outcome::Succeeded,
                Ok(None) if Instant::now() < give_up => {}
                _ => return Outcome::Failed,
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The sign-in is over: forget it, and its directory, and tell the status check.
    fn release(&self, login: &Login) {
        let mut live = locked(&self.live);
        if live
            .get(&login.engine)
            .is_some_and(|held| held.id == login.id)
        {
            live.remove(&login.engine);
        }
        drop(live);
        let _ = std::fs::remove_dir_all(&login.scratch);
        (self.ended)(login.engine.provider());
    }
}

/// A sign-in the holder owns. It ends when this drops.
struct Running {
    shared: Arc<Shared>,
    login: Arc<Login>,
    output: OutputSubscription,
    /// Set once the terminal's tree is confirmed gone.
    outcome: Option<Outcome>,
    /// How the engine ended by itself, once its output closed.
    natural: Option<Outcome>,
    /// Bytes the person has typed so far.
    typed: usize,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shared.end(&self.login, Outcome::Closed);
        self.shared.release(&self.login);
    }
}

/// What the terminal has produced since the last poll.
struct Polled {
    output: Vec<Vec<u8>>,
    /// Set once the terminal's output ended and its tree is confirmed gone, with all output handed
    /// over.
    ended: Option<Outcome>,
}

impl Running {
    fn poll(&mut self) -> Polled {
        if self.outcome.is_none() {
            let stopped = *locked(&self.login.stop);
            if let Some(reason) = stopped
                .or_else(|| (Instant::now() >= self.login.deadline).then_some(Outcome::TimedOut))
                && !self.login.closed.load(Ordering::Acquire)
            {
                self.shared.end(&self.login, reason);
            }
        }
        let mut output = Vec::new();
        while self.outcome.is_none() {
            match self.output.next(Duration::ZERO) {
                Output::Data(bytes) => output.push(bytes),
                Output::Idle => break,
                Output::Ended => {
                    self.finish();
                    break;
                }
                Output::Overflowed => {
                    locked(&self.login.stop).get_or_insert(Outcome::Failed);
                    self.finish();
                    break;
                }
            }
        }
        Polled {
            output,
            ended: self.outcome,
        }
    }

    /// The terminal's output closed: say how the engine ended, once its tree is confirmed gone. An
    /// unconfirmed close is tried again at the next poll and the page is told nothing yet.
    fn finish(&mut self) {
        let natural = *self
            .natural
            .get_or_insert_with(|| self.shared.exit_outcome(&self.login));
        if !self.shared.close(&self.login) {
            return;
        }
        self.shared.release(&self.login);
        let stopped = *locked(&self.login.stop);
        self.outcome = Some(stopped.unwrap_or(natural));
    }

    /// Hand the person's typing to the terminal, a few bytes at a time and only while it has room.
    /// A terminal that stays full for [`INPUT_WAIT`] is not reading, and the rest is dropped. This
    /// never waits on the engine, so nothing that ends the sign-in can sit behind it. `false`
    /// means the person has typed more than a sign-in takes.
    fn write(&mut self, mut bytes: &[u8]) -> bool {
        self.typed += bytes.len();
        if self.typed > MAX_TOTAL_INPUT_BYTES {
            return false;
        }
        let give_up = Instant::now() + INPUT_WAIT;
        while !bytes.is_empty() {
            match locked(&self.shared.host).write_ready(self.login.pty, bytes) {
                Ok(0) if Instant::now() < give_up => std::thread::sleep(Duration::from_millis(5)),
                Ok(0) | Err(_) => break,
                Ok(written) => bytes = &bytes[written..],
            }
        }
        true
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = locked(&self.shared.host).resize(self.login.pty, cols, rows);
    }
}

/// The environment a sign-in starts with: what a program needs to find its home, its terminal and
/// the network, and the engine's own configuration directory. Nothing else of the daemon's
/// environment, which can hold other programs' keys, reaches it.
fn login_env(env: &[EnvVar], engine: Engine) -> Vec<EnvVar> {
    const NAMES: &[&str] = &[
        "HOME",
        "PATH",
        "LANG",
        "TERM",
        "USER",
        "LOGNAME",
        "SHELL",
        "TMPDIR",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
    ];
    const PREFIXES: &[&str] = &["LC_", "XDG_"];
    let own: &[&str] = match engine {
        Engine::Claude => &["CLAUDE_CONFIG_DIR"],
        Engine::Codex => &["CODEX_HOME"],
        Engine::Pi => &["PI_CODING_AGENT_DIR"],
    };
    let mut kept: Vec<EnvVar> = env
        .iter()
        .filter(|var| {
            var.name().to_str().is_some_and(|name| {
                NAMES.contains(&name)
                    || own.contains(&name)
                    || PREFIXES.iter().any(|prefix| name.starts_with(prefix))
            })
        })
        .cloned()
        .collect();
    if !kept.iter().any(|var| var.name() == "TERM") {
        kept.push(EnvVar::new("TERM", "xterm-256color"));
    }
    kept
}

/// The fixed command for `engine`, from what the provider check found.
fn launch_spec(
    check: &ProviderCheck,
    envs: &BTreeMap<Provider, Vec<EnvVar>>,
    scratch: &Path,
    engine: Engine,
) -> Result<LaunchSpec, Refusal> {
    let provider = engine.provider();
    let probe = check
        .probes()
        .into_iter()
        .find(|probe| probe.provider == provider)
        .ok_or(Refusal::NotInstalled)?;
    let program = probe
        .command
        .filter(|program| probe.ok && program.is_absolute())
        .ok_or(Refusal::NotInstalled)?;
    let mut spec = LaunchSpec {
        argv: std::iter::once(program.into_os_string())
            .chain(engine.args().iter().map(Into::into))
            .collect(),
        env: login_env(envs.get(&provider).map_or(&[][..], Vec::as_slice), engine),
        cwd: scratch.to_path_buf(),
        stdin: LaunchInput::Null,
    };
    match probe.login_home {
        None => {}
        Some(dudes_proto::rpc::system::ProviderLoginHome::Ready { path })
            if engine == Engine::Codex =>
        {
            let plan = dudes_providers::codex::login::managed_login(&path)
                .map_err(|_| Refusal::Unavailable)?;
            spec.argv.truncate(1);
            spec.argv.extend(plan.args);
            spec.cwd = plan.cwd;
            spec.env.retain(|var| var.name() != "CODEX_HOME");
            spec.env.push(EnvVar::new("CODEX_HOME", path.as_os_str()));
        }
        Some(_) => return Err(Refusal::Unavailable),
    }
    Ok(spec)
}

#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum ClientMessage {
    #[serde(rename = "login.start")]
    Start {
        engine: Engine,
        cols: u16,
        rows: u16,
    },
    #[serde(rename = "login.write")]
    Write { data: String },
    #[serde(rename = "login.resize")]
    Resize { cols: u16, rows: u16 },
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "login.started")]
    Started,
    #[serde(rename = "login.refused")]
    Refused { reason: Refusal },
    #[serde(rename = "login.ended")]
    Ended { outcome: Outcome },
}

fn message_frame(message: &ServerMessage) -> Vec<u8> {
    text_frame(&serde_json::to_vec(message).expect("a sign-in message serializes"))
        .expect("a sign-in message fits one frame")
}

fn data_frame(bytes: &[u8]) -> Vec<u8> {
    text_frame(format!(r#"{{"type":"login.data","data":"{}"}}"#, base64(bytes)).as_bytes())
        .expect("one output chunk fits one WebSocket message")
}

enum Ending {
    /// A malformed or out-of-order client message.
    Policy,
    /// The sign-in ended and the page was told.
    Done,
    /// The page left, the session ended, the switch went off, or the daemon is stopping.
    Left,
    Failed,
}

impl From<io::Error> for Ending {
    fn from(_: io::Error) -> Self {
        Self::Failed
    }
}

type Flow = Result<(), Ending>;

/// Serve one admitted sign-in socket until it ends. Dropping the sign-in ends its process tree on
/// every way out.
pub(crate) fn serve(
    stream: &mut TcpStream,
    session: &LoginSession,
    buffered: &[u8],
    phase: &AtomicU8,
    logins: &EngineLogins,
    switches: &SharedSwitches,
) {
    let mut channel = Channel {
        session,
        logins,
        switches,
        running: None,
    };
    let Err(ending) = channel.drive(stream, buffered, phase);
    let frame = match ending {
        Ending::Policy => policy_close_frame(),
        Ending::Done | Ending::Left | Ending::Failed => close_frame(),
    };
    drop(channel);
    let _ = write_bytes(stream, &frame, phase);
}

struct Channel<'a> {
    session: &'a LoginSession,
    logins: &'a EngineLogins,
    switches: &'a SharedSwitches,
    running: Option<Running>,
}

impl Channel<'_> {
    fn drive(
        &mut self,
        stream: &mut TcpStream,
        buffered: &[u8],
        phase: &AtomicU8,
    ) -> Result<std::convert::Infallible, Ending> {
        stream.set_read_timeout(Some(ACCEPT_POLL))?;
        let mut decoder = Decoder::default();
        self.receive(decoder.push(buffered), stream, phase)?;
        let mut checked = Instant::now();
        let mut input = [0_u8; 8192];
        loop {
            if stop_requested(phase) {
                return Err(Ending::Left);
            }
            if checked.elapsed() >= RECHECK_AFTER {
                if !self.session.is_valid() || !self.switches.load().hire {
                    return Err(Ending::Left);
                }
                checked = Instant::now();
            }
            match stream.read(&mut input) {
                Ok(0) => return Err(Ending::Left),
                Ok(read) => self.receive(decoder.push(&input[..read]), stream, phase)?,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => return Err(error.into()),
            }
            self.pump(stream, phase)?;
        }
    }

    fn receive(
        &mut self,
        messages: Result<Vec<Message>, &'static str>,
        stream: &mut TcpStream,
        phase: &AtomicU8,
    ) -> Flow {
        for message in messages.map_err(|_| Ending::Policy)? {
            match message {
                Message::Text(text) => {
                    let client =
                        crate::floor_stream::decode_object(&text).map_err(|_| Ending::Policy)?;
                    self.client(client, stream, phase)?;
                }
                Message::Ping(bytes) => {
                    write_bytes(stream, &pong_frame(&bytes).ok_or(Ending::Policy)?, phase)?;
                }
                Message::Pong(_) => {}
                Message::Close => return Err(Ending::Left),
            }
        }
        Ok(())
    }

    fn client(&mut self, client: ClientMessage, stream: &mut TcpStream, phase: &AtomicU8) -> Flow {
        match (client, &mut self.running) {
            (ClientMessage::Start { engine, cols, rows }, None) => {
                if !terminal_size_in_bounds(cols, rows) || !self.session.touch() {
                    return Err(Ending::Policy);
                }
                let started = if engine == Engine::Pi && !self.switches.load().terminal {
                    Err(Refusal::NeedsTerminal)
                } else {
                    self.logins.start(engine, cols, rows)
                };
                match started {
                    Ok(running) => {
                        self.running = Some(running);
                        write_bytes(stream, &message_frame(&ServerMessage::Started), phase)?;
                        Ok(())
                    }
                    Err(reason) => {
                        write_bytes(
                            stream,
                            &message_frame(&ServerMessage::Refused { reason }),
                            phase,
                        )?;
                        Err(Ending::Done)
                    }
                }
            }
            (ClientMessage::Write { data }, Some(running)) => {
                let (bytes, _) = decode_terminal_input(&data)
                    .filter(|(bytes, _)| bytes.len() <= MAX_INPUT_BYTES)
                    .ok_or(Ending::Policy)?;
                if !self.session.touch() {
                    return Err(Ending::Left);
                }
                if running.write(&bytes) {
                    Ok(())
                } else {
                    Err(Ending::Policy)
                }
            }
            (ClientMessage::Resize { cols, rows }, Some(running)) => {
                if !terminal_size_in_bounds(cols, rows) {
                    return Err(Ending::Policy);
                }
                running.resize(cols, rows);
                Ok(())
            }
            _ => Err(Ending::Policy),
        }
    }

    fn pump(&mut self, stream: &mut TcpStream, phase: &AtomicU8) -> Flow {
        let Some(running) = &mut self.running else {
            return Ok(());
        };
        let polled = running.poll();
        for bytes in polled.output {
            write_bytes(stream, &data_frame(&bytes), phase)?;
        }
        if let Some(outcome) = polled.ended {
            write_bytes(
                stream,
                &message_frame(&ServerMessage::Ended { outcome }),
                phase,
            )?;
            return Err(Ending::Done);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(env: &[EnvVar]) -> Vec<String> {
        let mut names: Vec<String> = env
            .iter()
            .map(|var| var.name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_sign_in_gets_only_the_allowlisted_environment() {
        let daemon: Vec<EnvVar> = [
            "HOME",
            "PATH",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "TERM",
            "USER",
            "LOGNAME",
            "SHELL",
            "TMPDIR",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "no_proxy",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "PI_CODING_AGENT_DIR",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "DUDES_TOKEN",
            "DUDES_SOCK",
            "SSH_AUTH_SOCK",
            "ALL_PROXY",
        ]
        .into_iter()
        .map(|name| EnvVar::new(name, "v"))
        .collect();
        let common = [
            "HOME",
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "LOGNAME",
            "NO_PROXY",
            "PATH",
            "SHELL",
            "TERM",
            "TMPDIR",
            "USER",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "http_proxy",
            "https_proxy",
            "no_proxy",
        ];
        for (engine, own) in [
            (Engine::Claude, "CLAUDE_CONFIG_DIR"),
            (Engine::Codex, "CODEX_HOME"),
            (Engine::Pi, "PI_CODING_AGENT_DIR"),
        ] {
            let mut want: Vec<String> = common.iter().map(|name| (*name).to_owned()).collect();
            want.push(own.to_owned());
            want.sort();
            assert_eq!(names(&login_env(&daemon, engine)), want, "{engine:?}");
        }
    }

    #[test]
    fn a_sign_in_always_has_a_terminal_type() {
        let env = login_env(&[EnvVar::new("HOME", "/h")], Engine::Claude);
        assert_eq!(names(&env), ["HOME", "TERM"]);
    }
}
