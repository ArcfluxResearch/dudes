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
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
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
use crate::ports::{ClosedPtyHost, PtyHostPort};
use crate::provider_check::ProviderCheck;
use crate::pty_output::{Output, OutputSubscription};
use crate::runtime::terminal_size_in_bounds;
use crate::settings::{ENGINE_LOGIN_TIMEOUT, EngineLogin};

pub(crate) const PROTOCOL: &str = "dudes-engine-login-v1";

/// How long the daemon waits for the engine to report an exit after its output closes.
const EXIT_WAIT: Duration = Duration::from_secs(2);

const MAX_INPUT_BYTES: usize = 65_536;

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
}

type Launch = dyn Fn(Engine) -> Result<LaunchSpec, Refusal> + Send + Sync;

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
    timeout: Duration,
    host: Mutex<Box<dyn PtyHostPort>>,
    live: Mutex<BTreeMap<Engine, Arc<Login>>>,
    next: AtomicU64,
}

impl Shared {
    /// End every sign-in's process tree.
    fn stop(&self) {
        let _ = locked(&self.host).shutdown();
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
    /// The first reason anyone gave for ending it. The first reason wins.
    stop: Mutex<Option<Outcome>>,
}

fn locked<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl EngineLogins {
    pub(crate) fn new(
        launch: impl Fn(Engine) -> Result<LaunchSpec, Refusal> + Send + Sync + 'static,
        host: Box<dyn PtyHostPort>,
        timeout: Duration,
    ) -> Self {
        Self(Arc::new(Shared {
            launch: Box::new(launch),
            timeout,
            host: Mutex::new(host),
            live: Mutex::default(),
            next: AtomicU64::new(1),
        }))
    }

    /// The sign-ins the daemon runs: the provider check's program for each engine, in the
    /// environment a managed turn of it starts with, in a terminal host of their own.
    pub(crate) fn for_daemon(
        check: ProviderCheck,
        envs: BTreeMap<Provider, Vec<EnvVar>>,
        cwd: std::path::PathBuf,
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
            move |engine| launch_spec(&check, &envs, &cwd, engine),
            host,
            timeout,
        )
    }

    /// Start `engine`'s sign-in, ending any earlier one for it first. The returned handle ends
    /// the sign-in when it drops.
    fn start(&self, engine: Engine, cols: u16, rows: u16) -> Result<Running, Refusal> {
        let shared = &self.0;
        let spec = (shared.launch)(engine)?;
        let mut live = locked(&shared.live);
        if let Some(earlier) = live.remove(&engine) {
            shared.end(&earlier, Outcome::Replaced);
        }
        let mut host = locked(&shared.host);
        let opened = host
            .open(spec, cols, rows)
            .map_err(|_| Refusal::CouldNotStart)?;
        let Some(output) = host.subscribe(opened.pty) else {
            let _ = host.close(opened.pty);
            return Err(Refusal::CouldNotStart);
        };
        drop(host);
        let login = Arc::new(Login {
            id: shared.next.fetch_add(1, Ordering::Relaxed),
            engine,
            pty: opened.pty,
            deadline: Instant::now() + shared.timeout,
            stop: Mutex::new(None),
        });
        live.insert(engine, Arc::clone(&login));
        Ok(Running {
            shared: Arc::clone(shared),
            login,
            output,
            outcome: None,
        })
    }
}

impl Shared {
    /// Record why `login` is ending, then stop and reap its whole tree before returning.
    fn end(&self, login: &Login, reason: Outcome) {
        locked(&login.stop).get_or_insert(reason);
        let _ = locked(&self.host).close(login.pty);
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

    fn release(&self, login: &Login) {
        let mut live = locked(&self.live);
        if live
            .get(&login.engine)
            .is_some_and(|held| held.id == login.id)
        {
            live.remove(&login.engine);
        }
    }
}

/// A sign-in the holder owns. It ends when this drops.
struct Running {
    shared: Arc<Shared>,
    login: Arc<Login>,
    output: OutputSubscription,
    /// Set once the terminal's tree is gone.
    outcome: Option<Outcome>,
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
    /// Set once the terminal's output ended and its tree is gone, with all output handed over.
    ended: Option<Outcome>,
}

impl Running {
    fn poll(&mut self) -> Polled {
        if self.outcome.is_none() && Instant::now() >= self.login.deadline {
            self.shared.end(&self.login, Outcome::TimedOut);
        }
        let mut output = Vec::new();
        while self.outcome.is_none() {
            match self.output.next(Duration::ZERO) {
                Output::Data(bytes) => output.push(bytes),
                Output::Idle => break,
                Output::Ended => self.finish(),
                Output::Overflowed => {
                    locked(&self.login.stop).get_or_insert(Outcome::Failed);
                    self.finish();
                }
            }
        }
        Polled {
            output,
            ended: self.outcome,
        }
    }

    /// The terminal's output closed: say how the engine ended, with its tree gone.
    fn finish(&mut self) {
        let natural = self.shared.exit_outcome(&self.login);
        let _ = locked(&self.shared.host).close(self.login.pty);
        self.shared.release(&self.login);
        let stopped = *locked(&self.login.stop);
        self.outcome = Some(stopped.unwrap_or(natural));
    }

    fn write(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let written = locked(&self.shared.host).write(self.login.pty, bytes)?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            bytes = &bytes[written..];
        }
        Ok(())
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = locked(&self.shared.host).resize(self.login.pty, cols, rows);
    }
}

/// The fixed command for `engine`, from what the provider check found.
fn launch_spec(
    check: &ProviderCheck,
    envs: &BTreeMap<Provider, Vec<EnvVar>>,
    cwd: &std::path::Path,
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
    let mut env = envs.get(&provider).cloned().unwrap_or_default();
    crate::sign_in::strip_agent_vars(&mut env);
    if !env.iter().any(|var| var.name() == "TERM") {
        env.push(EnvVar::new("TERM", "xterm-256color"));
    }
    let mut spec = LaunchSpec {
        argv: std::iter::once(program.into_os_string())
            .chain(engine.args().iter().map(Into::into))
            .collect(),
        env,
        cwd: cwd.to_path_buf(),
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
        match (client, &self.running) {
            (ClientMessage::Start { engine, cols, rows }, None) => {
                if !terminal_size_in_bounds(cols, rows) || !self.session.touch() {
                    return Err(Ending::Policy);
                }
                match self.logins.start(engine, cols, rows) {
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
                running.write(&bytes)?;
                Ok(())
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
