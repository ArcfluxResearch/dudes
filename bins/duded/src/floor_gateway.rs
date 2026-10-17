//! The daemon-owned static Floor listener.
//!
//! Asset preparation is deliberately separate from listener construction: every file is checked
//! and retained in memory before one TCP port can become ready. Runtime requests therefore cross
//! no filesystem seam and can reach only the finite routes this catalog contains.
//!
//! The browser stream upgrades only `GET /floor/stream`. It requires the exact two offered
//! subprotocols `dudes-floor-v3` and `dudes-csrf.<43-character nonce>`, and echoes only the former.
//! Exact Host and Origin are checked before the host-only session cookie and CSRF carrier.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read, Write as _};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use dudes_supervisor::Clock;

use crate::floor_actions::{OnboardOutcome, OnboardRefusal, OnboardRequest};
use crate::floor_answer::{AnswerOutcome, AnswerRefusal, AnswerRequest, MAX_ANSWER_BODY_BYTES};
use crate::floor_chat::{ChatOutcome, ChatRefusal, ChatRequest, ChatVerb, MAX_CHAT_BODY_BYTES};
use crate::floor_control::{
    ControlOutcome, ControlRefusal, ControlRequest, ControlVerb, MAX_CONTROL_BODY_BYTES,
};
use crate::floor_diff::{DiffReads, DiffRequest, MAX_DIFF_BODY_BYTES, Reply as DiffReply};
use crate::floor_git::{GitReads, GitRequest, MAX_GIT_BODY_BYTES};
use crate::floor_goal::{GoalOutcome, GoalRefusal, GoalRequest, MAX_GOAL_BODY_BYTES};
use crate::floor_look::{LookOutcome, LookRefusal, LookRequest, MAX_LOOK_BODY_BYTES};
use crate::floor_memory::{MAX_MEMORY_BODY_BYTES, MemoryReads, MemoryRequest, MemoryRoute};
use crate::floor_rename::{MAX_RENAME_BODY_BYTES, RenameOutcome, RenameRefusal, RenameRequest};
use crate::floor_retire::{MAX_RETIRE_BODY_BYTES, RetireOutcome, RetireRefusal, RetireRequest};
use crate::floor_session::{
    ActionKind, FloorSessionAuthorizer, FloorSessions, ReserveResult, SESSION_COOKIE_NAME,
    StreamSession, TerminalSession,
};
use crate::floor_stream::{ClientMessage, FloorStreams, QueueRead, decode_client};
use crate::floor_ticket::{
    MAX_TICKET_BODY_BYTES, TicketLimits, TicketReply, TicketRequest, TicketRoute,
};
use crate::floor_websocket::{Decoder, Message, accept_key, close_frame, pong_frame};
use crate::settings::{FloorControl, FloorSettings};
use crate::{ExitCause, Handle};

mod recovery;
use recovery::RecoveryRoute;

/// The configured public root for the first static shell.
pub const BASE_PATH: &str = "/floor/";
/// The browser-wire generation required by the daemon and its matching Floor bundle.
pub const BROWSER_WIRE_VERSION: u32 = 3;

const MANIFEST_NAME: &str = "release-manifest.json";
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_ASSETS: usize = 256;
const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REQUEST_LINE: usize = 4096;
const MAX_REQUEST_HEAD: usize = 16 * 1024;
const MAX_REQUEST_DRAIN_BYTES: usize = 64 * 1024;
const REQUEST_DRAIN_DEADLINE: Duration = Duration::from_millis(200);
const MAX_BOOTSTRAP_BODY: usize = 4 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_HTTP_CONNECTIONS: usize = 32;
const STREAM_PATH: &str = "/floor/stream";
const RECOVERY_PATH: &str = "/floor/session/recover";
const INSPECT_RECOVERY_PATH: &str = "/floor/recovery/inspect";
const SUBMIT_RECOVERY_PATH: &str = "/floor/recovery/submit";
const RECEIPT_RECOVERY_PATH: &str = "/floor/recovery/receipt";
const ONBOARD_PATH: &str = "/floor/onboard";
/// Routed only when the operator enabled answering; otherwise a POST here is any
/// unrouted POST.
const ANSWER_PATH: &str = "/floor/answer";
/// Routed only when the operator enabled control; otherwise a POST to either is any
/// unrouted POST. No path names halt.
const PAUSE_PATH: &str = "/floor/pause";
const RESUME_PATH: &str = "/floor/resume";
/// Routed only when the operator enabled goal change; otherwise a POST here is any
/// unrouted POST.
const GOAL_PATH: &str = "/floor/goal";
/// Routed only when the operator enabled renewal beside goal change; otherwise a POST here is any
/// unrouted POST.
const RENEW_PATH: &str = "/floor/renew";
const RENAME_PATH: &str = "/floor/rename";
const LOOK_PATH: &str = "/floor/look";
/// Routed only when the operator enabled retiring; otherwise a POST here is any
/// unrouted POST.
const RETIRE_PATH: &str = "/floor/retire";
const TERMINAL_PATH: &str = "/floor/terminal";
/// Routed only when the operator enabled the Git tab; otherwise a POST here is any
/// unrouted POST.
const GIT_PATH: &str = "/floor/git";
/// Routed only when the operator enabled both the Git tab and the ticket diff; otherwise a POST
/// here is any unrouted POST.
const DIFF_PATH: &str = "/floor/diff";
/// Always routed: an agent's memory, read on demand.
const MEMORY_PAGE_PATH: &str = "/floor/memory/page";
const MEMORY_ENTRY_PATH: &str = "/floor/memory/entry";
const CHAT_SEND_PATH: &str = "/floor/chat/send";
const CHAT_CANCEL_PATH: &str = "/floor/chat/cancel";
/// The onboarding body cap.
const MAX_ONBOARD_BODY: usize = 4096;
const FLOOR_PROTOCOL: &str = "dudes-floor-v3";
const CSRF_PROTOCOL_PREFIX: &str = "dudes-csrf.";

/// Maximum delay between stop observation points in the idle accept loop.
pub const ACCEPT_POLL: Duration = Duration::from_millis(25);
/// Maximum time from accept through a complete request head.
pub const REQUEST_HEAD_DEADLINE: Duration = Duration::from_millis(500);
/// Maximum time spent writing one complete response.
pub const RESPONSE_WRITE_DEADLINE: Duration = Duration::from_millis(500);
/// Maximum time the constructor waits for the worker to acknowledge readiness.
pub const READY_BOUND: Duration = Duration::from_secs(1);
/// Maximum time shutdown waits for the worker to report completion.
pub const JOIN_BOUND: Duration = Duration::from_secs(2);
/// How long a fault handle waits for an injected handshake step. The worker's own bound starts
/// when the worker thread first runs after acknowledging readiness, which can be well after the
/// constructor returns on a loaded machine, so the wait must outlast it by more than a poll. A
/// satisfied wait returns at once.
const FAULT_WAIT: Duration = Duration::from_secs(5);

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";
/// A terminal-enabled daemon admits inline styles for the terminal's renderer. Every
/// other directive is byte-identical to [`CSP`].
const TERMINAL_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self'; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";

const STARTING: u8 = 0;
const READY: u8 = 1;
const STOP_REQUESTED: u8 = 2;

pub(crate) fn stop_requested(phase: &AtomicU8) -> bool {
    phase.load(Ordering::Acquire) == STOP_REQUESTED
}
const ENDED_BEFORE_READY: u8 = 3;

#[derive(Debug, Default)]
struct ConnectionSlots {
    live: AtomicUsize,
}

impl ConnectionSlots {
    fn acquire(self: &Arc<Self>) -> Option<ConnectionPermit> {
        let mut live = self.live.load(Ordering::Acquire);
        loop {
            if live >= MAX_HTTP_CONNECTIONS {
                return None;
            }
            match self.live.compare_exchange_weak(
                live,
                live + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(ConnectionPermit {
                        slots: Arc::clone(self),
                    });
                }
                Err(next) => live = next,
            }
        }
    }
}

struct ConnectionPermit {
    slots: Arc<ConnectionSlots>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.slots.live.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum CachePolicy {
    #[default]
    Unknown,
    NoStore,
    Immutable,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct ManifestFile {
    path: String,
    bytes: u64,
    sha256: String,
    cache: CachePolicy,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct ReleaseManifest {
    schema_version: u32,
    base_path: String,
    daemon_version: String,
    browser_wire_version: u32,
    asset_tree_sha256: String,
    files: Vec<ManifestFile>,
}

#[derive(Clone, Debug)]
pub(crate) struct Asset {
    pub(crate) bytes: Vec<u8>,
    pub(crate) content_type: &'static str,
    pub(crate) cache: &'static str,
}

/// A complete immutable catalog, ready to bind but owning no listener yet.
#[derive(Debug)]
pub struct PreparedAssets {
    pub(crate) assets: BTreeMap<String, Asset>,
    pub(crate) manifest: Vec<u8>,
    pub(crate) port: Option<u16>,
    /// The port the last run bound, asked for first when `port` is `None`.
    remembered: Option<u16>,
    /// Whether `POST /floor/answer` is routed at all: `[floor.answer] enabled`, fixed for the run.
    pub(crate) answer: bool,
    pub(crate) control: FloorControl,
    recovery: bool,
    pub(crate) terminal: bool,
    /// `Some` exactly when `[floor.git] enabled`: the route's reader and limits.
    git: Option<Arc<GitReads>>,
    /// `Some` exactly when `[floor.git]` and `[floor.diff]` are both enabled.
    diff: Option<Arc<DiffReads>>,
    /// `Some` exactly when `[floor.board] enabled`: the ticket routes' per-session limits.
    board: Option<Arc<TicketLimits>>,
}

impl PreparedAssets {
    /// Files admitted by this catalog, including the release manifest.
    #[must_use]
    pub fn release_files(&self) -> Vec<String> {
        std::iter::once(MANIFEST_NAME.to_owned())
            .chain(self.assets.keys().map(|route| {
                let path = route.strip_prefix(BASE_PATH).expect("prepared Floor route");
                if path.is_empty() {
                    "index.html".to_owned()
                } else {
                    path.to_owned()
                }
            }))
            .collect()
    }
}

/// A running Floor listener owned by one daemon.
#[derive(Debug)]
pub struct FloorGateway {
    addr: SocketAddr,
    sessions: FloorSessions,
    tickets: Option<Arc<TicketLimits>>,
    streams: FloorStreams,
    connection_slots: Arc<ConnectionSlots>,
    phase: Arc<AtomicU8>,
    stop: Sender<()>,
    ended: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

/// A release-unreachable fault controller for the integration tests.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct FloorStartFault(Arc<FaultInner>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FaultMode {
    Normal,
    WithholdReady,
    WorkerFirst,
    ParentFirst,
    WorkerReleaseTimeout,
    ParentReleaseTimeout,
    ConnectionSpawnFailure,
}

#[derive(Debug)]
struct FaultInner {
    mode: FaultMode,
    worker_ended: AtomicBool,
    unexpected: AtomicUsize,
    spawn_armed: AtomicBool,
    memory_interval: Option<Duration>,
    handshake: Mutex<Handshake>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct Handshake {
    worker_finalized: bool,
    parent_committed: bool,
    parent_release_observed: bool,
    timeout: Option<&'static str>,
}

impl FloorStartFault {
    fn new(mode: FaultMode) -> Self {
        Self::with(mode, None)
    }

    fn with(mode: FaultMode, memory_interval: Option<Duration>) -> Self {
        Self(Arc::new(FaultInner {
            mode,
            memory_interval,
            worker_ended: AtomicBool::new(false),
            unexpected: AtomicUsize::new(0),
            spawn_armed: AtomicBool::new(false),
            handshake: Mutex::new(Handshake::default()),
            changed: Condvar::new(),
        }))
    }

    /// Serve normally, with this much time required between two memory reads of one session.
    #[must_use]
    pub fn memory_interval(interval: Duration) -> Self {
        Self::with(FaultMode::Normal, Some(interval))
    }

    /// Serve normally until [`Self::fail_next_connection_spawn`] makes one connection thread fail.
    #[must_use]
    pub fn connection_spawn_failure() -> Self {
        Self::new(FaultMode::ConnectionSpawnFailure)
    }

    /// Arm the fatal connection-spawn failure the next accepted connection observes.
    pub fn fail_next_connection_spawn(&self) {
        self.0.spawn_armed.store(true, Ordering::Release);
    }

    fn connection_spawn_fails(&self) -> bool {
        self.0.mode == FaultMode::ConnectionSpawnFailure
            && self.0.spawn_armed.swap(false, Ordering::AcqRel)
    }

    /// Do not acknowledge readiness; constructor cleanup must stop the worker.
    #[must_use]
    pub fn withhold_ready() -> Self {
        Self::new(FaultMode::WithholdReady)
    }

    /// End after acknowledgement, before the parent is permitted to commit readiness.
    #[must_use]
    pub fn worker_first() -> Self {
        Self::new(FaultMode::WorkerFirst)
    }

    /// Hold the worker after acknowledgement until the parent commits readiness.
    #[must_use]
    pub fn parent_first() -> Self {
        Self::new(FaultMode::ParentFirst)
    }

    /// Mutation control: withhold the worker-finalized release from the waiting parent.
    #[must_use]
    pub fn worker_release_timeout() -> Self {
        Self::new(FaultMode::WorkerReleaseTimeout)
    }

    /// Mutation control: withhold the parent-committed release from the waiting worker.
    #[must_use]
    pub fn parent_release_timeout() -> Self {
        Self::new(FaultMode::ParentReleaseTimeout)
    }

    /// How many unexpected ready-worker ends reached the daemon handle.
    #[must_use]
    pub fn unexpected_reports(&self) -> usize {
        self.0.unexpected.load(Ordering::Acquire)
    }

    /// Whether the worker finalizer ran.
    #[must_use]
    pub fn worker_ended(&self) -> bool {
        self.0.worker_ended.load(Ordering::Acquire)
    }

    /// Wait for and return the named injected synchronization timeout.
    #[must_use]
    pub fn wait_timeout_name(&self) -> Option<&'static str> {
        let state = self
            .0
            .handshake
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (state, _) = self
            .0
            .changed
            .wait_timeout_while(state, FAULT_WAIT, |state| state.timeout.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        state.timeout
    }

    /// Wait for the worker to acknowledge the normal parent release.
    #[must_use]
    pub fn wait_parent_release_observed(&self) -> bool {
        let state = self
            .0
            .handshake
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (state, _) = self
            .0
            .changed
            .wait_timeout_while(state, FAULT_WAIT, |state| {
                !state.parent_release_observed && state.timeout.is_none()
            })
            .unwrap_or_else(PoisonError::into_inner);
        state.parent_release_observed
    }

    fn worker_finalized(&self) {
        let mut state = self
            .0
            .handshake
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.worker_finalized = true;
        self.0.changed.notify_all();
    }

    fn wait_for_worker_finalized(&self) -> Result<()> {
        let state = self
            .0
            .handshake
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (mut state, timed) = self
            .0
            .changed
            .wait_timeout_while(state, READY_BOUND, |state| !state.worker_finalized)
            .unwrap_or_else(PoisonError::into_inner);
        if timed.timed_out() && !state.worker_finalized {
            state.timeout = Some("worker-first finalization timed out");
            self.0.changed.notify_all();
            bail!("worker-first finalization timed out");
        }
        Ok(())
    }

    fn parent_committed(&self) {
        let mut state = self
            .0
            .handshake
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.parent_committed = true;
        self.0.changed.notify_all();
    }

    fn wait_for_parent_commit(&self) -> Result<()> {
        let state = self
            .0
            .handshake
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (mut state, timed) = self
            .0
            .changed
            .wait_timeout_while(state, READY_BOUND, |state| !state.parent_committed)
            .unwrap_or_else(PoisonError::into_inner);
        if timed.timed_out() && !state.parent_committed {
            state.timeout = Some("parent-first release timed out");
            self.0.changed.notify_all();
            bail!("parent-first release timed out");
        }
        Ok(())
    }

    fn observe_parent_release(&self) {
        let mut state = self
            .0
            .handshake
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.parent_release_observed = true;
        self.0.changed.notify_all();
    }
}

impl PreparedAssets {
    /// Route the Floor's answer action on this listener, or leave it an unrouted path.
    #[must_use]
    pub(crate) const fn with_answer(mut self, enabled: bool) -> Self {
        self.answer = enabled;
        self
    }

    /// Route the Floor's loop-control actions the operator enabled on this listener, and leave
    /// the rest unrouted paths.
    #[must_use]
    pub(crate) const fn with_control(mut self, control: FloorControl) -> Self {
        self.control = control;
        self
    }

    #[must_use]
    pub(crate) const fn with_recovery(mut self, enabled: bool) -> Self {
        self.recovery = enabled;
        self
    }

    #[must_use]
    pub(crate) const fn with_terminal(mut self, enabled: bool) -> Self {
        self.terminal = enabled;
        self
    }

    /// Route the seven `POST /floor/ticket` paths when `enabled`, and leave them unrouted paths
    /// otherwise.
    #[must_use]
    pub(crate) fn with_board(mut self, enabled: bool) -> Self {
        self.board = enabled.then(|| Arc::new(TicketLimits::new()));
        self
    }

    /// Route `POST /floor/git` over `reads`, and leave it an unrouted path otherwise.
    #[must_use]
    pub(crate) fn with_git(mut self, reads: Arc<GitReads>) -> Self {
        self.git = Some(reads);
        self
    }

    /// Route `POST /floor/diff` over `reads`, and leave it an unrouted path otherwise.
    #[must_use]
    pub(crate) fn with_diff(mut self, reads: Arc<DiffReads>) -> Self {
        self.diff = Some(reads);
        self
    }

    #[must_use]
    pub(crate) const fn with_remembered_port(mut self, port: Option<u16>) -> Self {
        self.remembered = port;
        self
    }

    /// Bind `127.0.0.1` on the configured port. With none configured, ask for the remembered
    /// port and take any free one when that fails.
    fn bind(&self) -> io::Result<TcpListener> {
        let on = |port| TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        match (self.port, self.remembered) {
            (Some(port), _) => on(port),
            (None, Some(port)) => on(port).or_else(|_| on(0)),
            (None, None) => on(0),
        }
    }

    /// Bind and start the listener, publishing ownership only after the ready handshake.
    pub fn start<C>(self, handle: Handle, clock: Arc<C>) -> Result<FloorGateway>
    where
        C: Clock + Send + Sync + 'static,
    {
        self.start_with_streams(
            handle,
            clock,
            FloorStreams::new(
                crate::floor::AgentSnapshot {
                    cards: Vec::new(),
                    activity: Vec::new(),
                    question_history: Vec::new(),
                    traces: Vec::new(),
                    mail: dudes_journal::projections::MailView::default(),
                    retired_agents: Vec::new(),
                    lsn: dudes_types::Lsn::ZERO,
                    agents: Vec::new(),
                    finished_today: None,
                },
                crate::floor_stream::FloorStreamFault::default(),
            )
            .expect("an empty Floor snapshot is serializable"),
        )
    }

    /// Bind with the post-started projection stream used by production boot.
    pub(crate) fn start_with_streams<C>(
        self,
        handle: Handle,
        clock: Arc<C>,
        streams: FloorStreams,
    ) -> Result<FloorGateway>
    where
        C: Clock + Send + Sync + 'static,
    {
        self.start_with_faults(
            handle,
            clock,
            streams,
            &FloorStartFault::new(FaultMode::Normal),
        )
    }

    /// The same constructor with a release-unreachable ordering controller.
    #[doc(hidden)]
    pub fn start_with_fault<C>(
        self,
        handle: Handle,
        clock: Arc<C>,
        fault: &FloorStartFault,
    ) -> Result<FloorGateway>
    where
        C: Clock + Send + Sync + 'static,
    {
        self.start_with_faults(
            handle,
            clock,
            FloorStreams::new(
                crate::floor::AgentSnapshot {
                    cards: Vec::new(),
                    activity: Vec::new(),
                    question_history: Vec::new(),
                    traces: Vec::new(),
                    mail: dudes_journal::projections::MailView::default(),
                    retired_agents: Vec::new(),
                    lsn: dudes_types::Lsn::ZERO,
                    agents: Vec::new(),
                    finished_today: None,
                },
                crate::floor_stream::FloorStreamFault::default(),
            )
            .expect("an empty Floor snapshot is serializable"),
            fault,
        )
    }

    /// The production stream constructor with the release-unreachable start controller.
    #[doc(hidden)]
    #[expect(
        clippy::too_many_lines,
        reason = "one ready handshake publishes the listener and its run-local session state atomically"
    )]
    pub(crate) fn start_with_faults<C>(
        self,
        handle: Handle,
        clock: Arc<C>,
        streams: FloorStreams,
        fault: &FloorStartFault,
    ) -> Result<FloorGateway>
    where
        C: Clock + Send + Sync + 'static,
    {
        let listener = self
            .bind()
            .context("binding the Floor listener on 127.0.0.1")?;
        let addr = listener
            .local_addr()
            .context("reading the Floor listener address")?;
        listener
            .set_nonblocking(true)
            .context("making the Floor listener nonblocking")?;

        let phase = Arc::new(AtomicU8::new(STARTING));
        let (stop, stops) = mpsc::channel();
        let (ready, readiness) = mpsc::channel();
        let (ended, endings) = mpsc::channel();
        let thread_phase = Arc::clone(&phase);
        let thread_fault = fault.clone();
        let assets = Arc::new(self.assets);
        let manifest = Arc::new(self.manifest);
        let answer = self.answer;
        let control = self.control;
        let recovery = self.recovery && control.pause_resume();
        let terminal = self.terminal;
        let git = self.git;
        let diff = self.diff;
        let board = self.board;
        let tickets = board.clone();
        let memory = fault
            .0
            .memory_interval
            .map_or_else(MemoryReads::new, MemoryReads::with_interval);
        let thread_clock = Arc::clone(&clock);
        let sessions = FloorSessions::new(addr, clock);
        let thread_sessions = sessions.clone();
        let thread_streams = streams.clone();
        let connection_failure = handle.clone();
        let connection_slots = Arc::new(ConnectionSlots::default());
        let thread_connection_slots = Arc::clone(&connection_slots);
        let thread = std::thread::Builder::new()
            .name("dudes-floor".to_owned())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    serve(
                        &listener,
                        &stops,
                        &ready,
                        Catalog {
                            assets: &assets,
                            manifest: &manifest,
                            answer,
                            control,
                            recovery,
                            terminal,
                            git: git.as_deref(),
                            diff: diff.as_deref(),
                            board: board.as_deref(),
                            memory: &memory,
                        },
                        &thread_sessions,
                        &thread_streams,
                        thread_clock.as_ref(),
                        &thread_phase,
                        &thread_fault,
                        &connection_failure,
                        &thread_connection_slots,
                    )
                }));
                let previous = thread_phase.compare_exchange(
                    STARTING,
                    ENDED_BEFORE_READY,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                thread_sessions.invalidate();
                if thread_fault.0.mode != FaultMode::WorkerReleaseTimeout {
                    thread_fault.worker_finalized();
                }
                match previous {
                    Err(READY) => {
                        thread_fault.0.unexpected.fetch_add(1, Ordering::AcqRel);
                        handle.stop(ExitCause::FloorGatewayEnded);
                    }
                    Ok(_) | Err(STOP_REQUESTED | ENDED_BEFORE_READY) => {}
                    Err(other) => tracing::error!(phase = other, "invalid Floor listener phase"),
                }
                if let Ok(Err(error)) = outcome {
                    tracing::error!(%error, "the Floor listener failed");
                }
                thread_fault.0.worker_ended.store(true, Ordering::Release);
                let _ = ended.send(());
            })
            .context("spawning the Floor listener thread")?;

        match readiness.recv_timeout(READY_BOUND) {
            Ok(()) => {
                if matches!(
                    fault.0.mode,
                    FaultMode::WorkerFirst | FaultMode::WorkerReleaseTimeout
                ) && let Err(error) = fault.wait_for_worker_finalized()
                {
                    finish_local_start(&stop, &endings, thread, &phase);
                    return Err(error);
                }
                match phase.compare_exchange(STARTING, READY, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => {
                        if fault.0.mode == FaultMode::ParentFirst {
                            fault.parent_committed();
                        }
                        Ok(FloorGateway {
                            addr,
                            sessions,
                            tickets,
                            streams,
                            connection_slots,
                            phase,
                            stop,
                            ended: endings,
                            thread: Some(thread),
                        })
                    }
                    Err(ENDED_BEFORE_READY) => {
                        finish_local_start(&stop, &endings, thread, &phase);
                        bail!("Floor listener ended before readiness was committed")
                    }
                    Err(other) => {
                        finish_local_start(&stop, &endings, thread, &phase);
                        bail!("Floor listener readiness entered invalid phase {other}")
                    }
                }
            }
            Err(error) => {
                finish_local_start(&stop, &endings, thread, &phase);
                match error {
                    RecvTimeoutError::Timeout => bail!("Floor listener readiness timed out"),
                    RecvTimeoutError::Disconnected => {
                        bail!("Floor listener ended before acknowledging readiness")
                    }
                }
            }
        }
    }
}

impl FloorGateway {
    /// The exact loopback address the operating system bound.
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The run-local read-only session gate, without its mint or exchange operations.
    #[must_use]
    pub fn session_authorizer(&self) -> FloorSessionAuthorizer {
        self.sessions.authorizer()
    }

    pub(crate) fn recovery_authority(&self) -> crate::floor_session::FloorRecoveryAuthority {
        self.sessions.recovery_authority()
    }

    /// A read-only integration-test observation of live connection ownership.
    #[doc(hidden)]
    #[must_use]
    pub fn probe(&self) -> FloorGatewayProbe {
        FloorGatewayProbe {
            slots: Arc::clone(&self.connection_slots),
        }
    }

    /// The sessions and the counts that follow them, for `dudes upgrade` to carry across a restart.
    #[must_use]
    pub(crate) fn carry(
        &self,
        checks: crate::baseline::CheckRuns,
    ) -> crate::floor_session::FloorCarry {
        crate::floor_session::FloorCarry::new(self.sessions.clone(), self.tickets.clone(), checks)
    }

    /// Stop admission and wait only to the finite completion bound.
    /// Returns whether the listener thread joined without a panic.
    pub fn stop(&mut self) -> bool {
        self.sessions.invalidate();
        self.streams.end();
        request_stop(&self.phase);
        let _ = self.stop.send(());
        match self.ended.recv_timeout(JOIN_BOUND) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(thread) = self.thread.take() {
                    return thread.join().is_ok();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                tracing::error!(addr = %self.addr, "the Floor listener did not stop within its bound");
                drop(self.thread.take());
                return false;
            }
        }
        true
    }
}

/// Release-unreachable observation of the gateway's owned connection count.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct FloorGatewayProbe {
    slots: Arc<ConnectionSlots>,
}

impl FloorGatewayProbe {
    /// Connections that own one of the accepted 32 slots now.
    #[must_use]
    pub fn live_connections(&self) -> usize {
        self.slots.live.load(Ordering::Acquire)
    }
}

impl Drop for FloorGateway {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop();
        }
    }
}

fn finish_local_start(
    stop: &Sender<()>,
    ended: &Receiver<()>,
    thread: JoinHandle<()>,
    phase: &AtomicU8,
) {
    request_stop(phase);
    let _ = stop.send(());
    match ended.recv_timeout(JOIN_BOUND) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => {
            let _ = thread.join();
        }
        Err(RecvTimeoutError::Timeout) => drop(thread),
    }
}

/// Make the run terminal for every already-upgraded connection before the scope joins it.
///
/// `std::thread::scope` joins every outstanding scoped thread before it returns, so a fatal
/// accept or spawn failure that returned first would wait on peers whose session and stream are
/// still valid — up to the idle bound, past the daemon's own stop. The daemon is told before any
/// permit is released, so the cause it exits for is this failure and not a later stop.
fn terminate_child_handlers(
    failure: &Handle,
    sessions: &FloorSessions,
    streams: &FloorStreams,
    phase: &AtomicU8,
) {
    failure.stop(ExitCause::FloorGatewayEnded);
    sessions.invalidate();
    streams.end();
    request_stop(phase);
}

fn request_stop(phase: &AtomicU8) {
    let mut current = phase.load(Ordering::Acquire);
    while matches!(current, STARTING | READY) {
        match phase.compare_exchange(current, STOP_REQUESTED, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(next) => current = next,
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the owned listener thread receives each lifecycle and immutable serving input once"
)]
fn serve(
    listener: &TcpListener,
    stops: &Receiver<()>,
    ready: &Sender<()>,
    catalog: Catalog<'_>,
    sessions: &FloorSessions,
    streams: &FloorStreams,
    clock: &(dyn Clock + Send + Sync),
    phase: &AtomicU8,
    fault: &FloorStartFault,
    connection_failure: &Handle,
    slots: &Arc<ConnectionSlots>,
) -> Result<()> {
    if fault.0.mode == FaultMode::WithholdReady {
        // Hold the readiness sender until the parent gives up and stops the worker. A timed wait
        // here could end first on a slow machine and report a disconnect instead of a timeout.
        let _ = stops.recv();
        return Ok(());
    }
    ready
        .send(())
        .context("acknowledging Floor listener readiness")?;
    match fault.0.mode {
        FaultMode::WorkerFirst => return Ok(()),
        FaultMode::WorkerReleaseTimeout => {
            let _ = stops.recv_timeout(READY_BOUND + ACCEPT_POLL);
            return Ok(());
        }
        FaultMode::ParentFirst | FaultMode::ParentReleaseTimeout => {
            fault.wait_for_parent_commit()?;
            fault.observe_parent_release();
            return Ok(());
        }
        FaultMode::Normal | FaultMode::WithholdReady | FaultMode::ConnectionSpawnFailure => {}
    }

    std::thread::scope(|scope| -> Result<()> {
        loop {
            match stops.recv_timeout(ACCEPT_POLL) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
                Err(RecvTimeoutError::Timeout) => {}
            }
            for _ in 0..MAX_HTTP_CONNECTIONS {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let Some(permit) = slots.acquire() else {
                            let _ = stream.shutdown(Shutdown::Both);
                            continue;
                        };
                        if stream.set_nonblocking(false).is_err() {
                            drop(permit);
                            let _ = stream.shutdown(Shutdown::Both);
                            continue;
                        }
                        let failure = connection_failure.clone();
                        let spawned = if fault.connection_spawn_fails() {
                            Err(io::Error::other("injected Floor connection thread failure"))
                        } else {
                            std::thread::Builder::new()
                                .name("dudes-floor-connection".to_owned())
                                .spawn_scoped(scope, move || {
                                    let outcome = std::panic::catch_unwind(
                                        std::panic::AssertUnwindSafe(|| {
                                            handle_connection(
                                                stream, catalog, sessions, streams, clock, phase,
                                                &failure,
                                            );
                                        }),
                                    );
                                    drop(permit);
                                    if outcome.is_err() {
                                        failure.stop(ExitCause::FloorGatewayEnded);
                                    }
                                })
                                .map(drop)
                        };
                        if let Err(error) = spawned {
                            terminate_child_handlers(connection_failure, sessions, streams, phase);
                            return Err(error).context("spawning a Floor connection thread");
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => {
                        terminate_child_handlers(connection_failure, sessions, streams, phase);
                        return Err(error).context("accepting a Floor connection");
                    }
                }
            }
        }
    })
}

fn handle_connection(
    mut stream: TcpStream,
    catalog: Catalog<'_>,
    sessions: &FloorSessions,
    streams: &FloorStreams,
    clock: &(dyn Clock + Send + Sync),
    phase: &AtomicU8,
    handle: &Handle,
) {
    match read_request(&mut stream, catalog, sessions, phase, handle) {
        Ok(Some(ConnectionAction::Response(response))) => {
            if write_response(&mut stream, &response, catalog.csp(), phase).is_ok()
                && response.code >= 400
                && stream.shutdown(Shutdown::Write).is_ok()
            {
                discard_request_remainder(&mut stream, phase);
            }
        }
        Ok(Some(ConnectionAction::Upgrade(Upgrade {
            accept,
            session,
            buffered,
        }))) => {
            if session.is_valid()
                && write_upgrade(&mut stream, &accept, session.route().protocol(), phase).is_ok()
            {
                match session {
                    SocketSession::Stream(session) => {
                        let _ = serve_websocket(
                            &mut stream,
                            &session,
                            &buffered,
                            streams,
                            clock,
                            phase,
                            handle,
                        );
                    }
                    SocketSession::Terminal(session) => {
                        crate::floor_terminal::serve(
                            &mut stream,
                            session,
                            &buffered,
                            phase,
                            handle,
                        );
                    }
                }
            }
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, "Floor connection setup failed"),
    }
}

#[derive(Clone, Copy)]
struct Catalog<'a> {
    assets: &'a BTreeMap<String, Asset>,
    manifest: &'a [u8],
    answer: bool,
    control: FloorControl,
    recovery: bool,
    terminal: bool,
    git: Option<&'a GitReads>,
    diff: Option<&'a DiffReads>,
    board: Option<&'a TicketLimits>,
    memory: &'a MemoryReads,
}

impl Catalog<'_> {
    /// The switch is read once at start, so one run stamps one CSP.
    const fn csp(self) -> &'static str {
        if self.terminal { TERMINAL_CSP } else { CSP }
    }
}

/// Which browser action a routed POST carries. The path alone decides it; no body member does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActionRoute {
    Onboard,
    Answer,
    Control(ControlVerb),
    Goal,
    Renew,
    Rename,
    Look,
    Retire,
    Git,
    Diff,
    Memory(MemoryRoute),
    Chat(ChatVerb),
    Ticket(TicketRoute),
    Recovery(RecoveryRoute),
}

impl ActionRoute {
    fn of(method: &str, path: &str, catalog: Catalog<'_>) -> Option<Self> {
        if method != "POST" {
            return None;
        }
        match path {
            CHAT_SEND_PATH => Some(Self::Chat(ChatVerb::Send)),
            CHAT_CANCEL_PATH => Some(Self::Chat(ChatVerb::Cancel)),
            ONBOARD_PATH => Some(Self::Onboard),
            INSPECT_RECOVERY_PATH if catalog.recovery => {
                Some(Self::Recovery(RecoveryRoute::Inspect))
            }
            SUBMIT_RECOVERY_PATH if catalog.recovery => Some(Self::Recovery(RecoveryRoute::Submit)),
            RECEIPT_RECOVERY_PATH => Some(Self::Recovery(RecoveryRoute::Receipt)),
            ANSWER_PATH if catalog.answer => Some(Self::Answer),
            PAUSE_PATH if catalog.control.pause_resume() => Some(Self::Control(ControlVerb::Pause)),
            RESUME_PATH if catalog.control.pause_resume() => {
                Some(Self::Control(ControlVerb::Resume))
            }
            GOAL_PATH if catalog.control.goal() => Some(Self::Goal),
            RENEW_PATH if catalog.control.renew() => Some(Self::Renew),
            RENAME_PATH if catalog.control.pause_resume() => Some(Self::Rename),
            LOOK_PATH if catalog.control.pause_resume() => Some(Self::Look),
            RETIRE_PATH if catalog.control.retire() => Some(Self::Retire),
            GIT_PATH if catalog.git.is_some() => Some(Self::Git),
            DIFF_PATH if catalog.diff.is_some() => Some(Self::Diff),
            MEMORY_PAGE_PATH => Some(Self::Memory(MemoryRoute::Page)),
            MEMORY_ENTRY_PATH => Some(Self::Memory(MemoryRoute::Entry)),
            _ => catalog.board.and(TicketRoute::of(path)).map(Self::Ticket),
        }
    }

    /// The `Content-Length` cap, checked before a byte of the body is read.
    const fn max_body(self) -> usize {
        match self {
            Self::Onboard => MAX_ONBOARD_BODY,
            Self::Recovery(_) => crate::floor_recovery::MAX_RECOVERY_BODY_BYTES,
            Self::Answer => MAX_ANSWER_BODY_BYTES,
            Self::Control(_) => MAX_CONTROL_BODY_BYTES,
            Self::Goal | Self::Renew => MAX_GOAL_BODY_BYTES,
            Self::Rename => MAX_RENAME_BODY_BYTES,
            Self::Look => MAX_LOOK_BODY_BYTES,
            Self::Retire => MAX_RETIRE_BODY_BYTES,
            Self::Git => MAX_GIT_BODY_BYTES,
            Self::Diff => MAX_DIFF_BODY_BYTES,
            Self::Memory(_) => MAX_MEMORY_BODY_BYTES,
            Self::Chat(_) => MAX_CHAT_BODY_BYTES,
            Self::Ticket(_) => MAX_TICKET_BODY_BYTES,
        }
    }
}

/// One authenticated browser action's admitted head, carried to its body and session checks.
struct ActionHead {
    content_length: usize,
    host: String,
    origin: String,
    cookie: String,
    csrf: String,
}

struct Response<'a> {
    code: u16,
    reason: &'static str,
    body: Cow<'a, [u8]>,
    content_type: Option<&'static str>,
    cache: &'static str,
    allow: Option<&'static str>,
    set_cookie: Option<String>,
}

enum ConnectionAction<'a> {
    Response(Response<'a>),
    Upgrade(Upgrade),
}

struct Upgrade {
    accept: String,
    session: SocketSession,
    buffered: Vec<u8>,
}

enum SocketSession {
    Stream(StreamSession),
    Terminal(TerminalSession),
}

impl SocketSession {
    const fn route(&self) -> SocketRoute {
        match self {
            Self::Stream(_) => SocketRoute::Stream,
            Self::Terminal(_) => SocketRoute::Terminal,
        }
    }

    fn is_valid(&self) -> bool {
        match self {
            Self::Stream(session) => session.is_valid(),
            Self::Terminal(session) => session.is_valid(),
        }
    }
}

#[derive(Clone, Copy)]
enum SocketRoute {
    Stream,
    Terminal,
}

impl SocketRoute {
    fn of(method: &str, path: &str, catalog: Catalog<'_>) -> Option<Self> {
        match (method, path) {
            ("GET", STREAM_PATH) => Some(Self::Stream),
            ("GET", TERMINAL_PATH) if catalog.terminal => Some(Self::Terminal),
            _ => None,
        }
    }

    const fn protocol(self) -> &'static str {
        match self {
            Self::Stream => FLOOR_PROTOCOL,
            Self::Terminal => crate::floor_terminal::TERMINAL_PROTOCOL,
        }
    }
}

enum Parsed<'a> {
    Response(Response<'a>),
    Exchange {
        content_length: usize,
        host: String,
        origin: String,
    },
    Recover {
        host: String,
        origin: String,
        cookie: String,
    },
    Action {
        route: ActionRoute,
        head: ActionHead,
    },
    Upgrade {
        accept: String,
        session: SocketSession,
    },
}

trait RequestReader: Read {
    fn set_request_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
}

impl RequestReader for TcpStream {
    fn set_request_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one bounded request-head loop dispatches every finite Floor route"
)]
fn read_request<'a>(
    stream: &mut impl RequestReader,
    catalog: Catalog<'a>,
    sessions: &FloorSessions,
    phase: &AtomicU8,
    handle: &Handle,
) -> io::Result<Option<ConnectionAction<'a>>> {
    let deadline = Instant::now() + REQUEST_HEAD_DEADLINE;
    let mut head = Vec::with_capacity(MAX_REQUEST_HEAD);
    let mut chunk = [0_u8; 1024];
    loop {
        if stop_requested(phase) {
            return Ok(None);
        }
        if let Some(end) = find_head_end(&head) {
            if end > MAX_REQUEST_HEAD {
                return Ok(Some(ConnectionAction::Response(error_response(
                    431,
                    "Request Header Fields Too Large",
                ))));
            }
            return match parse_request(&head[..end], catalog, sessions) {
                Parsed::Response(response) => Ok(Some(ConnectionAction::Response(response))),
                Parsed::Exchange {
                    content_length,
                    host,
                    origin,
                } => {
                    let Some(body) =
                        read_body(stream, &head[end..], content_length, deadline, phase)?
                    else {
                        return Ok(None);
                    };
                    Ok(Some(ConnectionAction::Response(exchange_response(
                        sessions, &host, &origin, &body,
                    ))))
                }
                Parsed::Recover {
                    host,
                    origin,
                    cookie,
                } => {
                    if !head[end..].is_empty() {
                        return Ok(Some(ConnectionAction::Response(error_response(
                            400,
                            "Bad Request",
                        ))));
                    }
                    Ok(Some(ConnectionAction::Response(recover_response(
                        sessions, &host, &origin, &cookie,
                    ))))
                }
                Parsed::Action {
                    route,
                    head: action,
                } => {
                    if head[end..].len() > action.content_length {
                        return Ok(Some(ConnectionAction::Response(error_response(
                            400,
                            "Bad Request",
                        ))));
                    }
                    let Some(body) =
                        read_body(stream, &head[end..], action.content_length, deadline, phase)?
                    else {
                        return Ok(None);
                    };
                    Ok(Some(ConnectionAction::Response(match route {
                        ActionRoute::Recovery(route) => {
                            recovery::response(route, sessions, handle, &action, &body)
                        }
                        ActionRoute::Onboard => onboard_response(sessions, handle, &action, &body),
                        ActionRoute::Answer => answer_response(sessions, handle, &action, &body),
                        ActionRoute::Control(verb) => {
                            control_response(verb, sessions, handle, &action, &body)
                        }
                        ActionRoute::Goal => {
                            goal_response(GoalRoute::Change, sessions, handle, &action, &body)
                        }
                        ActionRoute::Renew => {
                            goal_response(GoalRoute::Renew, sessions, handle, &action, &body)
                        }
                        ActionRoute::Rename => rename_response(sessions, handle, &action, &body),
                        ActionRoute::Look => look_response(sessions, handle, &action, &body),
                        ActionRoute::Retire => retire_response(sessions, handle, &action, &body),
                        ActionRoute::Git => {
                            return Ok(git_response(
                                sessions, handle, catalog, phase, &action, &body,
                            )
                            .map(ConnectionAction::Response));
                        }
                        ActionRoute::Diff => {
                            return Ok(diff_response(
                                sessions, handle, catalog, phase, &action, &body,
                            )
                            .map(ConnectionAction::Response));
                        }
                        ActionRoute::Memory(route) => {
                            memory_response(route, sessions, handle, catalog.memory, &action, &body)
                        }
                        ActionRoute::Chat(verb) => {
                            chat_response(verb, sessions, handle, &action, &body)
                        }
                        ActionRoute::Ticket(route) => {
                            ticket_response(route, sessions, handle, catalog, &action, &body)
                        }
                    })))
                }
                Parsed::Upgrade { accept, session } => {
                    Ok(Some(ConnectionAction::Upgrade(Upgrade {
                        accept,
                        session,
                        buffered: head[end..].to_vec(),
                    })))
                }
            };
        }
        if request_line_len(&head).is_some_and(|len| len > MAX_REQUEST_LINE) {
            return Ok(Some(ConnectionAction::Response(error_response(
                414,
                "URI Too Long",
            ))));
        }
        if head.len() >= MAX_REQUEST_HEAD {
            return Ok(Some(ConnectionAction::Response(error_response(
                431,
                "Request Header Fields Too Large",
            ))));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        let remaining = deadline.saturating_duration_since(now);
        stream.set_request_timeout(Some(remaining))?;
        let room = (MAX_REQUEST_HEAD + 1 - head.len()).min(chunk.len());
        match stream.read(&mut chunk[..room]) {
            Ok(0) => {
                return Ok(Some(ConnectionAction::Response(error_response(
                    400,
                    "Bad Request",
                ))));
            }
            Ok(read) => head.extend_from_slice(&chunk[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Ok(None);
            }
            Err(_) => return Ok(None),
        }
    }
}

fn read_body(
    stream: &mut impl RequestReader,
    buffered: &[u8],
    content_length: usize,
    deadline: Instant,
    phase: &AtomicU8,
) -> io::Result<Option<Vec<u8>>> {
    let mut body = Vec::with_capacity(content_length);
    body.extend_from_slice(&buffered[..buffered.len().min(content_length)]);
    while body.len() < content_length {
        if stop_requested(phase) {
            return Ok(None);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        stream.set_request_timeout(Some(deadline.saturating_duration_since(now)))?;
        let mut chunk = [0_u8; 1024];
        let room = (content_length - body.len()).min(chunk.len());
        match stream.read(&mut chunk[..room]) {
            Ok(0) => return Ok(None),
            Ok(read) => body.extend_from_slice(&chunk[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(Some(body))
}

fn parse_request<'a>(head: &[u8], catalog: Catalog<'a>, sessions: &FloorSessions) -> Parsed<'a> {
    if !strict_crlf(head) {
        return Parsed::Response(error_response(400, "Bad Request"));
    }
    let line_len = request_line_len(head).unwrap_or(head.len());
    if line_len > MAX_REQUEST_LINE {
        return Parsed::Response(error_response(414, "URI Too Long"));
    }
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        Err(httparse::Error::TooManyHeaders) => {
            return Parsed::Response(error_response(431, "Request Header Fields Too Large"));
        }
        Ok(httparse::Status::Partial) | Err(_) => {
            return Parsed::Response(error_response(400, "Bad Request"));
        }
    }
    let Some(method) = request.method else {
        return Parsed::Response(error_response(400, "Bad Request"));
    };
    let Some(path) = request.path else {
        return Parsed::Response(error_response(400, "Bad Request"));
    };
    if method == "POST" && path == format!("{BASE_PATH}session") {
        return exchange_request(request.version, request.headers, sessions);
    }
    if method == "POST" && path == RECOVERY_PATH {
        return recover_request(request.version, request.headers);
    }
    if let Some(route) = ActionRoute::of(method, path, catalog) {
        return action_request(route, request.version, request.headers, sessions);
    }
    if let Some(route) = SocketRoute::of(method, path, catalog) {
        return upgrade_request(route, request.version, request.headers, sessions);
    }
    if !valid_static_framing(request.version, request.headers) {
        return Parsed::Response(error_response(400, "Bad Request"));
    }
    if method != "GET" {
        return Parsed::Response(Response {
            allow: Some("GET"),
            ..error_response(405, "Method Not Allowed")
        });
    }
    if path == format!("{BASE_PATH}{MANIFEST_NAME}") {
        return Parsed::Response(Response {
            code: 200,
            reason: "OK",
            body: Cow::Borrowed(catalog.manifest),
            content_type: Some("application/json"),
            cache: "no-store",
            allow: None,
            set_cookie: None,
        });
    }
    Parsed::Response(catalog.assets.get(path).map_or_else(
        || error_response(404, "Not Found"),
        |asset| Response {
            code: 200,
            reason: "OK",
            body: Cow::Borrowed(&asset.bytes),
            content_type: Some(asset.content_type),
            cache: asset.cache,
            allow: None,
            set_cookie: None,
        },
    ))
}

fn exchange_request(
    version: Option<u8>,
    headers: &[httparse::Header<'_>],
    sessions: &FloorSessions,
) -> Parsed<'static> {
    if version != Some(1) || forbidden_framing(headers) {
        return Parsed::Response(error_response(400, "Bad Request"));
    }
    let Some(host) = one_header(headers, "host").and_then(|value| std::str::from_utf8(value).ok())
    else {
        return Parsed::Response(error_response(400, "Bad Request"));
    };
    let Some(origin) =
        one_header(headers, "origin").and_then(|value| std::str::from_utf8(value).ok())
    else {
        return Parsed::Response(error_response(403, "Forbidden"));
    };
    if !sessions.authority_matches(host, origin) {
        return Parsed::Response(error_response(403, "Forbidden"));
    }
    let Some(content_type) = one_header(headers, "content-type") else {
        return Parsed::Response(error_response(400, "Bad Request"));
    };
    if !content_type.eq_ignore_ascii_case(b"text/plain;charset=UTF-8") {
        return Parsed::Response(error_response(400, "Bad Request"));
    }
    let Some(content_length) = one_header(headers, "content-length")
        .filter(|value| !value.is_empty() && value.iter().all(u8::is_ascii_digit))
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return Parsed::Response(error_response(400, "Bad Request"));
    };
    if content_length > MAX_BOOTSTRAP_BODY {
        return Parsed::Response(error_response(413, "Content Too Large"));
    }
    Parsed::Exchange {
        content_length,
        host: host.to_owned(),
        origin: origin.to_owned(),
    }
}

fn exchange_response<'a>(
    sessions: &FloorSessions,
    host: &str,
    origin: &str,
    body: &[u8],
) -> Response<'a> {
    let Some(exchange) = sessions.exchange(host, origin, body) else {
        return error_response(403, "Forbidden");
    };
    let body = serde_json::to_vec(&serde_json::json!({ "csrf": exchange.csrf }))
        .expect("a string-only Floor session response serializes");
    Response {
        code: 200,
        reason: "OK",
        body: Cow::Owned(body),
        content_type: Some("application/json"),
        cache: "no-store",
        allow: None,
        set_cookie: Some(format!(
            "{SESSION_COOKIE_NAME}={}; Path={BASE_PATH}; HttpOnly; SameSite=Strict; Max-Age=28800",
            exchange.cookie
        )),
    }
}

fn recover_request(version: Option<u8>, headers: &[httparse::Header<'_>]) -> Parsed<'static> {
    if version != Some(1) || forbidden_framing(headers) {
        return Parsed::Response(error_response(400, "Bad Request"));
    }
    let Ok(Some(content_length)) = unique_header(headers, "content-length") else {
        return Parsed::Response(error_response(400, "Bad Request"));
    };
    if content_length != b"0" {
        return Parsed::Response(error_response(400, "Bad Request"));
    }
    let host = match unique_header(headers, "host") {
        Ok(Some(value)) => value,
        Ok(None) => return Parsed::Response(error_response(403, "Forbidden")),
        Err(()) => return Parsed::Response(error_response(400, "Bad Request")),
    };
    let origin = match unique_header(headers, "origin") {
        Ok(Some(value)) => value,
        Ok(None) => return Parsed::Response(error_response(403, "Forbidden")),
        Err(()) => return Parsed::Response(error_response(400, "Bad Request")),
    };
    let cookie = match unique_header(headers, "cookie") {
        Ok(Some(value)) => value,
        Ok(None) => return Parsed::Response(error_response(403, "Forbidden")),
        Err(()) => return Parsed::Response(error_response(400, "Bad Request")),
    };
    let Some(host) = std::str::from_utf8(host).ok() else {
        return Parsed::Response(error_response(403, "Forbidden"));
    };
    let Some(origin) = std::str::from_utf8(origin).ok() else {
        return Parsed::Response(error_response(403, "Forbidden"));
    };
    let Some(cookie) = std::str::from_utf8(cookie).ok().and_then(session_cookie) else {
        return Parsed::Response(error_response(403, "Forbidden"));
    };
    Parsed::Recover {
        host: host.to_owned(),
        origin: origin.to_owned(),
        cookie: cookie.to_owned(),
    }
}

fn recover_response<'a>(
    sessions: &FloorSessions,
    host: &str,
    origin: &str,
    cookie: &str,
) -> Response<'a> {
    let Some(recovery) = sessions.recover(host, origin, cookie) else {
        return error_response(403, "Forbidden");
    };
    let body = serde_json::to_vec(&serde_json::json!({ "csrf": recovery.csrf }))
        .expect("a string-only Floor recovery response serializes");
    Response {
        code: 200,
        reason: "OK",
        body: Cow::Owned(body),
        content_type: Some("application/json"),
        cache: "no-store",
        allow: None,
        set_cookie: None,
    }
}

/// The transport admission every browser action shares:
/// HTTP/1.1; one Host, Origin, Cookie and CSRF each, the first two exactly this listener's; JSON;
/// and a decimal `Content-Length` within the route's cap, refused with `413` before any body is
/// read.
fn action_request(
    route: ActionRoute,
    version: Option<u8>,
    headers: &[httparse::Header<'_>],
    sessions: &FloorSessions,
) -> Parsed<'static> {
    if version != Some(1) || forbidden_framing(headers) {
        return Parsed::Response(error_response(400, "Bad Request"));
    }
    let host = match unique_header(headers, "host") {
        Ok(Some(value)) => value,
        Ok(None) => return Parsed::Response(error_response(403, "Forbidden")),
        Err(()) => return Parsed::Response(error_response(400, "Bad Request")),
    };
    let origin = match unique_header(headers, "origin") {
        Ok(Some(value)) => value,
        Ok(None) => return Parsed::Response(error_response(403, "Forbidden")),
        Err(()) => return Parsed::Response(error_response(400, "Bad Request")),
    };
    let cookie = match unique_header(headers, "cookie") {
        Ok(Some(value)) => value,
        Ok(None) => return Parsed::Response(error_response(403, "Forbidden")),
        Err(()) => return Parsed::Response(error_response(400, "Bad Request")),
    };
    let csrf = match unique_header(headers, crate::floor_session::CSRF_HEADER_NAME) {
        Ok(Some(value)) => value,
        Ok(None) => return Parsed::Response(error_response(403, "Forbidden")),
        Err(()) => return Parsed::Response(error_response(400, "Bad Request")),
    };
    let Some(host) = std::str::from_utf8(host).ok() else {
        return Parsed::Response(error_response(403, "Forbidden"));
    };
    let Some(origin) = std::str::from_utf8(origin).ok() else {
        return Parsed::Response(error_response(403, "Forbidden"));
    };
    if !sessions.authority_matches(host, origin) {
        return Parsed::Response(error_response(403, "Forbidden"));
    }
    let Some(cookie) = std::str::from_utf8(cookie).ok().and_then(session_cookie) else {
        return Parsed::Response(error_response(403, "Forbidden"));
    };
    let Some(csrf) = std::str::from_utf8(csrf)
        .ok()
        .filter(|value| valid_nonce(value))
    else {
        return Parsed::Response(error_response(403, "Forbidden"));
    };
    if matches!(route, ActionRoute::Chat(_))
        && sessions
            .authorize_onboard(host, origin, cookie, csrf)
            .is_none()
    {
        return Parsed::Response(error_response(403, "Forbidden"));
    }
    let Some(content_type) = one_header(headers, "content-type") else {
        return Parsed::Response(error_response(400, "Bad Request"));
    };
    if !content_type.eq_ignore_ascii_case(b"application/json") {
        return Parsed::Response(error_response(400, "Bad Request"));
    }
    let Some(content_length) = one_header(headers, "content-length")
        .filter(|value| !value.is_empty() && value.iter().all(u8::is_ascii_digit))
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return Parsed::Response(error_response(400, "Bad Request"));
    };
    if content_length > route.max_body() {
        return Parsed::Response(error_response(413, "Content Too Large"));
    }
    Parsed::Action {
        route,
        head: ActionHead {
            content_length,
            host: host.to_owned(),
            origin: origin.to_owned(),
            cookie: cookie.to_owned(),
            csrf: csrf.to_owned(),
        },
    }
}

fn onboard_response<'a>(
    sessions: &FloorSessions,
    handle: &Handle,
    head: &ActionHead,
    body: &[u8],
) -> Response<'a> {
    let Some(session) =
        sessions.authorize_onboard(&head.host, &head.origin, &head.cookie, &head.csrf)
    else {
        return error_response(403, "Forbidden");
    };
    let Ok(request) = OnboardRequest::parse(body) else {
        return error_response(400, "Bad Request");
    };
    let attempt_wire = request.attempt_wire.clone();
    let mut reservation = match session.reserve(
        ActionKind::Onboard,
        request.attempt_id,
        request.request_digest,
    ) {
        ReserveResult::Owner(reservation) => reservation,
        ReserveResult::Pending => {
            return action_json(
                202,
                "Accepted",
                &serde_json::json!({
                    "attempt_id": attempt_wire, "state": "pending"
                }),
            );
        }
        ReserveResult::Conflict => return error_response(409, "Conflict"),
        ReserveResult::Forbidden => return error_response(403, "Forbidden"),
    };
    let outcome = handle.onboard(session, request);
    match outcome {
        Ok(OnboardOutcome::Complete {
            attempt_id,
            agent,
            goal,
        }) => action_json(
            200,
            "OK",
            &serde_json::json!({
                "attempt_id":attempt_id,"state":"complete","agent":agent,"goal":goal
            }),
        ),
        Ok(OnboardOutcome::Partial {
            attempt_id,
            agent,
            reason,
            current_goal,
            retired,
        }) => action_json(
            409,
            "Conflict",
            &serde_json::json!({
                "attempt_id":attempt_id,"state":"hired_without_goal","agent":agent,
                "goal":null,"reason":reason,"current_goal":current_goal,"retired":retired
            }),
        ),
        Err(OnboardRefusal::Forbidden) => error_response(403, "Forbidden"),
        Err(OnboardRefusal::Conflict) => error_response(409, "Conflict"),
        Err(OnboardRefusal::Unavailable) => {
            reservation.retain_pending();
            error_response(503, "Service Unavailable")
        }
        Err(OnboardRefusal::UnavailableNoEffect) => error_response(503, "Service Unavailable"),
        Err(OnboardRefusal::InvalidInput) => error_response(400, "Bad Request"),
        Err(OnboardRefusal::ProfileChanged) => action_json(
            409,
            "Conflict",
            &serde_json::json!({
                "attempt_id": attempt_wire, "state": "refused", "reason": "profile_changed"
            }),
        ),
        Err(OnboardRefusal::Check(reason)) => action_json(
            409,
            "Conflict",
            &serde_json::json!({ "attempt_id": attempt_wire, "state": "refused", "reason": reason }),
        ),
        Err(OnboardRefusal::SharedFolder(refusal)) => action_json(
            409,
            "Conflict",
            &serde_json::json!({
                "attempt_id": attempt_wire, "state": "refused", "reason": "shared_folder",
                "note": refusal.to_string()
            }),
        ),
    }
}

/// The connection-thread steps, then the writer's job; each outcome is one
/// of the fixed refusal reasons or the applied result. Only an uncertain append keeps its reservation.
fn answer_response<'a>(
    sessions: &FloorSessions,
    handle: &Handle,
    head: &ActionHead,
    body: &[u8],
) -> Response<'a> {
    let Some(session) =
        sessions.authorize_onboard(&head.host, &head.origin, &head.cookie, &head.csrf)
    else {
        return error_response(403, "Forbidden");
    };
    let Ok(request) = AnswerRequest::parse(body) else {
        return error_response(400, "Bad Request");
    };
    let request_wire = request.request_wire.clone();
    let mut reservation = match session.reserve(
        ActionKind::Answer,
        request.request_id,
        request.request_digest,
    ) {
        ReserveResult::Owner(reservation) => reservation,
        ReserveResult::Pending => {
            return action_json(
                202,
                "Accepted",
                &serde_json::json!({ "request_id": request_wire, "state": "pending" }),
            );
        }
        ReserveResult::Conflict => return error_response(409, "Conflict"),
        ReserveResult::Forbidden => return error_response(403, "Forbidden"),
    };
    match handle.answer(session, request) {
        Ok(AnswerOutcome::Answered {
            request_id,
            agent,
            question_lsn,
        }) => action_json(
            200,
            "OK",
            &serde_json::json!({
                "request_id": request_id, "state": "answered", "agent": agent,
                "question_lsn": question_lsn.0.to_string()
            }),
        ),
        Ok(AnswerOutcome::Refused { request_id, reason }) => action_json(
            409,
            "Conflict",
            &serde_json::json!({ "request_id": request_id, "state": "refused", "reason": reason }),
        ),
        Err(AnswerRefusal::Forbidden) => error_response(403, "Forbidden"),
        Err(AnswerRefusal::Conflict) => error_response(409, "Conflict"),
        Err(AnswerRefusal::Unavailable) => {
            reservation.retain_pending();
            error_response(503, "Service Unavailable")
        }
        Err(AnswerRefusal::UnavailableNoEffect) => error_response(503, "Service Unavailable"),
    }
}

/// The connection-thread steps, then the writer's job; each outcome is one
/// of the fixed refusal reasons or the applied result. Both routes share one reservation namespace, so an identity in flight on one
/// meets the other as a conflict. Only an uncertain append keeps its reservation.
fn control_response<'a>(
    verb: ControlVerb,
    sessions: &FloorSessions,
    handle: &Handle,
    head: &ActionHead,
    body: &[u8],
) -> Response<'a> {
    let Some(session) =
        sessions.authorize_onboard(&head.host, &head.origin, &head.cookie, &head.csrf)
    else {
        return error_response(403, "Forbidden");
    };
    let Ok(request) = ControlRequest::parse(verb, body) else {
        return error_response(400, "Bad Request");
    };
    let request_wire = request.request_wire.clone();
    let mut reservation = match session.reserve(
        ActionKind::Control,
        request.request_id,
        request.request_digest,
    ) {
        ReserveResult::Owner(reservation) => reservation,
        ReserveResult::Pending => {
            return action_json(
                202,
                "Accepted",
                &serde_json::json!({ "request_id": request_wire, "state": "pending" }),
            );
        }
        ReserveResult::Conflict => return error_response(409, "Conflict"),
        ReserveResult::Forbidden => return error_response(403, "Forbidden"),
    };
    match handle.control(session, request) {
        Ok(ControlOutcome::Applied {
            request_id,
            verb,
            agent,
            control_lsn,
        }) => action_json(
            200,
            "OK",
            &serde_json::json!({
                "request_id": request_id, "state": verb.applied(), "agent": agent,
                "control_lsn": control_lsn.0.to_string()
            }),
        ),
        Ok(ControlOutcome::Refused { request_id, reason }) => action_json(
            409,
            "Conflict",
            &serde_json::json!({ "request_id": request_id, "state": "refused", "reason": reason }),
        ),
        Err(ControlRefusal::Forbidden) => error_response(403, "Forbidden"),
        Err(ControlRefusal::Conflict) => error_response(409, "Conflict"),
        Err(ControlRefusal::Unavailable) => {
            reservation.retain_pending();
            error_response(503, "Service Unavailable")
        }
        Err(ControlRefusal::UnavailableNoEffect) => error_response(503, "Service Unavailable"),
    }
}

fn rename_response<'a>(
    sessions: &FloorSessions,
    handle: &Handle,
    head: &ActionHead,
    body: &[u8],
) -> Response<'a> {
    let Some(session) =
        sessions.authorize_onboard(&head.host, &head.origin, &head.cookie, &head.csrf)
    else {
        return error_response(403, "Forbidden");
    };
    let Ok(request) = RenameRequest::parse(body) else {
        return error_response(400, "Bad Request");
    };
    let request_wire = request.request_wire.clone();
    let mut reservation = match session.reserve(
        ActionKind::Rename,
        request.request_id,
        request.request_digest,
    ) {
        ReserveResult::Owner(reservation) => reservation,
        ReserveResult::Pending => {
            return action_json(
                202,
                "Accepted",
                &serde_json::json!({ "request_id": request_wire, "state": "pending" }),
            );
        }
        ReserveResult::Conflict => return error_response(409, "Conflict"),
        ReserveResult::Forbidden => return error_response(403, "Forbidden"),
    };
    match handle.rename(session, request) {
        Ok(RenameOutcome::Changed {
            request_id,
            agent,
            name,
        }) => action_json(
            200,
            "OK",
            &serde_json::json!({
                "request_id": request_id, "state": "changed", "agent": agent, "name": name
            }),
        ),
        Ok(RenameOutcome::Refused { request_id, reason }) => action_json(
            409,
            "Conflict",
            &serde_json::json!({ "request_id": request_id, "state": "refused", "reason": reason }),
        ),
        Err(RenameRefusal::Forbidden) => error_response(403, "Forbidden"),
        Err(RenameRefusal::Conflict) => error_response(409, "Conflict"),
        Err(RenameRefusal::Unavailable) => {
            reservation.retain_pending();
            error_response(503, "Service Unavailable")
        }
        Err(RenameRefusal::UnavailableNoEffect) => error_response(503, "Service Unavailable"),
    }
}

fn look_response<'a>(
    sessions: &FloorSessions,
    handle: &Handle,
    head: &ActionHead,
    body: &[u8],
) -> Response<'a> {
    let Some(session) =
        sessions.authorize_onboard(&head.host, &head.origin, &head.cookie, &head.csrf)
    else {
        return error_response(403, "Forbidden");
    };
    let Ok(request) = LookRequest::parse(body) else {
        return error_response(400, "Bad Request");
    };
    let request_wire = request.request_wire.clone();
    let mut reservation =
        match session.reserve(ActionKind::Look, request.request_id, request.request_digest) {
            ReserveResult::Owner(reservation) => reservation,
            ReserveResult::Pending => {
                return action_json(
                    202,
                    "Accepted",
                    &serde_json::json!({ "request_id": request_wire, "state": "pending" }),
                );
            }
            ReserveResult::Conflict => return error_response(409, "Conflict"),
            ReserveResult::Forbidden => return error_response(403, "Forbidden"),
        };
    match handle.look(session, request) {
        Ok(LookOutcome::Changed {
            request_id,
            agent,
            role,
            character,
            accent,
        }) => action_json(
            200,
            "OK",
            &serde_json::json!({
                "request_id": request_id, "state": "changed", "agent": agent,
                "role": role, "character": character, "accent": accent
            }),
        ),
        Ok(LookOutcome::Refused { request_id, reason }) => action_json(
            409,
            "Conflict",
            &serde_json::json!({ "request_id": request_id, "state": "refused", "reason": reason }),
        ),
        Err(LookRefusal::Forbidden) => error_response(403, "Forbidden"),
        Err(LookRefusal::Conflict) => error_response(409, "Conflict"),
        Err(LookRefusal::Unavailable) => {
            reservation.retain_pending();
            error_response(503, "Service Unavailable")
        }
        Err(LookRefusal::UnavailableNoEffect) => error_response(503, "Service Unavailable"),
    }
}

/// The two goal routes: one body shape, reservation kind and success state each.
#[derive(Clone, Copy)]
enum GoalRoute {
    Change,
    Renew,
}

/// The connection-thread steps, then the writer's job; each outcome is one
/// of the fixed refusal reasons or the applied result. Only an uncertain append keeps its reservation.
fn goal_response<'a>(
    route: GoalRoute,
    sessions: &FloorSessions,
    handle: &Handle,
    head: &ActionHead,
    body: &[u8],
) -> Response<'a> {
    let Some(session) =
        sessions.authorize_onboard(&head.host, &head.origin, &head.cookie, &head.csrf)
    else {
        return error_response(403, "Forbidden");
    };
    let (parsed, kind, applied) = match route {
        GoalRoute::Change => (GoalRequest::parse(body), ActionKind::Goal, "changed"),
        GoalRoute::Renew => (
            GoalRequest::parse_renewal(body),
            ActionKind::Renew,
            "renewed",
        ),
    };
    let Ok(request) = parsed else {
        return error_response(400, "Bad Request");
    };
    let request_wire = request.request_wire.clone();
    let mut reservation = match session.reserve(kind, request.request_id, request.request_digest) {
        ReserveResult::Owner(reservation) => reservation,
        ReserveResult::Pending => {
            return action_json(
                202,
                "Accepted",
                &serde_json::json!({ "request_id": request_wire, "state": "pending" }),
            );
        }
        ReserveResult::Conflict => return error_response(409, "Conflict"),
        ReserveResult::Forbidden => return error_response(403, "Forbidden"),
    };
    match handle.goal(session, request) {
        Ok(GoalOutcome::Applied {
            request_id,
            agent,
            goal,
        }) => action_json(
            200,
            "OK",
            &serde_json::json!({
                "request_id": request_id, "state": applied, "agent": agent, "goal": goal
            }),
        ),
        Ok(GoalOutcome::Refused { request_id, reason }) => action_json(
            409,
            "Conflict",
            &serde_json::json!({ "request_id": request_id, "state": "refused", "reason": reason }),
        ),
        Err(GoalRefusal::Forbidden) => error_response(403, "Forbidden"),
        Err(GoalRefusal::Conflict) => error_response(409, "Conflict"),
        Err(GoalRefusal::Unavailable) => {
            reservation.retain_pending();
            error_response(503, "Service Unavailable")
        }
        Err(GoalRefusal::UnavailableNoEffect) => error_response(503, "Service Unavailable"),
    }
}

/// The connection-thread steps, then the writer's job; each outcome is one
/// of the fixed refusal reasons or the applied result. Only an uncertain append keeps its reservation.
fn retire_response<'a>(
    sessions: &FloorSessions,
    handle: &Handle,
    head: &ActionHead,
    body: &[u8],
) -> Response<'a> {
    let Some(session) =
        sessions.authorize_onboard(&head.host, &head.origin, &head.cookie, &head.csrf)
    else {
        return error_response(403, "Forbidden");
    };
    let Ok(request) = RetireRequest::parse(body) else {
        return error_response(400, "Bad Request");
    };
    let request_wire = request.request_wire.clone();
    let mut reservation = match session.reserve(
        ActionKind::Retire,
        request.request_id,
        request.request_digest,
    ) {
        ReserveResult::Owner(reservation) => reservation,
        ReserveResult::Pending => {
            return action_json(
                202,
                "Accepted",
                &serde_json::json!({ "request_id": request_wire, "state": "pending" }),
            );
        }
        ReserveResult::Conflict => return error_response(409, "Conflict"),
        ReserveResult::Forbidden => return error_response(403, "Forbidden"),
    };
    match handle.retire(session, request) {
        Ok(RetireOutcome::Retired {
            request_id,
            agent,
            retire_lsn,
        }) => action_json(
            200,
            "OK",
            &serde_json::json!({
                "request_id": request_id, "state": "retired", "agent": agent,
                "retire_lsn": retire_lsn.0.to_string()
            }),
        ),
        Ok(RetireOutcome::Refused { request_id, reason }) => action_json(
            409,
            "Conflict",
            &serde_json::json!({ "request_id": request_id, "state": "refused", "reason": reason }),
        ),
        Err(RetireRefusal::Forbidden) => error_response(403, "Forbidden"),
        Err(RetireRefusal::Conflict) => error_response(409, "Conflict"),
        Err(RetireRefusal::Unavailable) => {
            reservation.retain_pending();
            error_response(503, "Service Unavailable")
        }
        Err(RetireRefusal::UnavailableNoEffect) => error_response(503, "Service Unavailable"),
    }
}

/// On this connection thread: the session and the body are checked, the read runs on a
/// worker this thread waits for, and every outcome is one finite [`dudes_proto::GitView`]. The read
/// changes nothing, so it keeps no request identity and writes no journal record.
fn git_response<'a>(
    sessions: &FloorSessions,
    handle: &Handle,
    catalog: Catalog<'_>,
    phase: &AtomicU8,
    head: &ActionHead,
    body: &[u8],
) -> Option<Response<'a>> {
    let reads = catalog.git?;
    let Some(session) =
        sessions.authorize_onboard(&head.host, &head.origin, &head.cookie, &head.csrf)
    else {
        return Some(error_response(403, "Forbidden"));
    };
    let Ok(request) = GitRequest::parse(body) else {
        return Some(error_response(400, "Bad Request"));
    };
    let view = match reads.serve(
        session.limit_key(),
        || handle.git_target(request.agent),
        || stop_requested(phase),
    ) {
        Ok(view) => view?,
        Err(_) => return Some(error_response(503, "Service Unavailable")),
    };
    Some(action_json(
        200,
        "OK",
        &serde_json::to_value(&view).expect("a finite Git view serializes"),
    ))
}

/// On this connection thread: the session and the body are checked, the read runs on a worker
/// this thread waits for, and the reply is charged to the session before any byte of it is sent.
/// Every refusal is one finite reply with no repository data. A session that ends while the read
/// runs gets `403` and no reply.
fn diff_response<'a>(
    sessions: &FloorSessions,
    handle: &Handle,
    catalog: Catalog<'_>,
    phase: &AtomicU8,
    head: &ActionHead,
    body: &[u8],
) -> Option<Response<'a>> {
    let reads = catalog.diff?;
    let Some(session) =
        sessions.authorize_onboard(&head.host, &head.origin, &head.cookie, &head.csrf)
    else {
        return Some(error_response(403, "Forbidden"));
    };
    let Ok(request) = DiffRequest::parse(body) else {
        return Some(error_response(400, "Bad Request"));
    };
    match reads.serve(
        session.limit_key(),
        &request,
        |card| handle.diff_recorded(card),
        || stop_requested(phase),
        || session.is_valid(),
    ) {
        DiffReply::Body(bytes) => Some(json_response(200, "OK", bytes)),
        DiffReply::Stopping => None,
        DiffReply::Revoked => Some(error_response(403, "Forbidden")),
        DiffReply::Unavailable => Some(error_response(503, "Service Unavailable")),
    }
}

/// The session and body checks, then the session's interval, then one read on the loop, then the
/// session again, because it can expire or be revoked while the loop answers. A loop that is
/// ending drops the read, and the route answers `503`.
fn memory_response<'a>(
    route: MemoryRoute,
    sessions: &FloorSessions,
    handle: &Handle,
    reads: &MemoryReads,
    head: &ActionHead,
    body: &[u8],
) -> Response<'a> {
    let Some(session) =
        sessions.authorize_onboard(&head.host, &head.origin, &head.cookie, &head.csrf)
    else {
        return error_response(403, "Forbidden");
    };
    let Ok(request) = MemoryRequest::parse(route, body) else {
        return error_response(400, "Bad Request");
    };
    let reply = if reads.begin(session.limit_key()) {
        match handle.floor_memory(request) {
            Some(_) if !session.is_valid() => return error_response(403, "Forbidden"),
            Some(answer) => request.reply(&answer),
            None => return error_response(503, "Service Unavailable"),
        }
    } else {
        request.busy()
    };
    json_response(200, "OK", reply)
}

/// The session and body checks, then the session's limits, then one request on the writer. A
/// retry after success finds the card on a newer sequence and is refused `stale_card`, so no
/// request carries an identity.
fn ticket_response<'a>(
    route: TicketRoute,
    sessions: &FloorSessions,
    handle: &Handle,
    catalog: Catalog<'_>,
    head: &ActionHead,
    body: &[u8],
) -> Response<'a> {
    // A lost answer to a create is not retried; Remove clears a duplicate.
    let Some(limits) = catalog.board else {
        return error_response(403, "Forbidden");
    };
    let Some(session) =
        sessions.authorize_onboard(&head.host, &head.origin, &head.cookie, &head.csrf)
    else {
        return error_response(403, "Forbidden");
    };
    let Ok(request) = TicketRequest::parse(route, body) else {
        return error_response(400, "Bad Request");
    };
    let turn = match limits.begin(session.limit_key(), request.kind()) {
        Ok(turn) => turn,
        Err(reason) => {
            return action_json(
                409,
                "Conflict",
                &serde_json::json!({"state": "refused", "reason": reason}),
            );
        }
    };
    match handle.ticket(session, request) {
        TicketReply::Done(value) => {
            turn.succeeded();
            action_json(200, "OK", &value)
        }
        TicketReply::Refused(reason) => action_json(
            409,
            "Conflict",
            &serde_json::json!({"state": "refused", "reason": reason}),
        ),
        TicketReply::Invalid => error_response(400, "Bad Request"),
        TicketReply::Forbidden => error_response(403, "Forbidden"),
        TicketReply::Unavailable => error_response(503, "Service Unavailable"),
    }
}

fn chat_response<'a>(
    verb: ChatVerb,
    sessions: &FloorSessions,
    handle: &Handle,
    head: &ActionHead,
    body: &[u8],
) -> Response<'a> {
    let Some(session) =
        sessions.authorize_onboard(&head.host, &head.origin, &head.cookie, &head.csrf)
    else {
        return error_response(403, "Forbidden");
    };
    let Ok(request) = ChatRequest::parse(verb, body) else {
        return error_response(400, "Bad Request");
    };
    let mut reservation = match session.reserve(
        ActionKind::Chat,
        request.request_id_digest,
        request.request_digest,
    ) {
        ReserveResult::Owner(reservation) => reservation,
        ReserveResult::Pending => {
            return action_json(
                202,
                "Accepted",
                &serde_json::json!({
                    "request_id":request.request_wire,"state":"pending"
                }),
            );
        }
        ReserveResult::Conflict => return error_response(409, "Conflict"),
        ReserveResult::Forbidden => return error_response(403, "Forbidden"),
    };
    match handle.chat(session, request) {
        Ok(outcome) => {
            let (request_id, state, agent, note_lsn) = match outcome {
                ChatOutcome::Queued {
                    request_id,
                    agent,
                    note_lsn,
                } => (request_id, "queued", agent, note_lsn),
                ChatOutcome::Cancelled {
                    request_id,
                    agent,
                    note_lsn,
                } => (request_id, "cancelled", agent, note_lsn),
                ChatOutcome::ReissueCancelled {
                    request_id,
                    agent,
                    note_lsn,
                } => (request_id, "reissue_cancelled", agent, note_lsn),
                ChatOutcome::ContextLost {
                    request_id,
                    agent,
                    turn_id,
                } => {
                    return action_json(
                        409,
                        "Conflict",
                        &serde_json::json!({"state":"context_lost","request_id":request_id,"agent":agent,"turn_id":turn_id}),
                    );
                }
                ChatOutcome::Refused { request_id, reason } => {
                    return action_json(
                        409,
                        "Conflict",
                        &serde_json::json!({
                            "request_id":request_id,"state":"refused","reason":reason
                        }),
                    );
                }
            };
            action_json(
                200,
                "OK",
                &serde_json::json!({
                    "request_id":request_id,"state":state,"agent":agent,"note_lsn":note_lsn.0.to_string()
                }),
            )
        }
        Err(ChatRefusal::Forbidden) => error_response(403, "Forbidden"),
        Err(ChatRefusal::Conflict) => error_response(409, "Conflict"),
        Err(ChatRefusal::Unavailable) => {
            reservation.retain_pending();
            error_response(503, "Service Unavailable")
        }
        Err(ChatRefusal::UnavailableNoEffect) => error_response(503, "Service Unavailable"),
    }
}

fn action_json<'a>(code: u16, reason: &'static str, value: &serde_json::Value) -> Response<'a> {
    json_response(
        code,
        reason,
        serde_json::to_vec(value).expect("finite browser action outcome"),
    )
}

const fn json_response<'a>(code: u16, reason: &'static str, body: Vec<u8>) -> Response<'a> {
    Response {
        code,
        reason,
        body: Cow::Owned(body),
        content_type: Some("application/json"),
        cache: "no-store",
        allow: None,
        set_cookie: None,
    }
}

fn upgrade_request(
    route: SocketRoute,
    version: Option<u8>,
    headers: &[httparse::Header<'_>],
    sessions: &FloorSessions,
) -> Parsed<'static> {
    if version != Some(1)
        || headers.iter().any(|header| {
            header.name.eq_ignore_ascii_case("transfer-encoding")
                || header.name.eq_ignore_ascii_case("expect")
                || header.name.eq_ignore_ascii_case("content-length")
        })
    {
        return Parsed::Response(error_response(400, "Bad Request"));
    }
    let Some(host) = header_text(headers, "host") else {
        return Parsed::Response(error_response(400, "Bad Request"));
    };
    let Some(origin) = header_text(headers, "origin") else {
        return Parsed::Response(error_response(403, "Forbidden"));
    };
    if !sessions.authority_matches(host, origin) {
        return Parsed::Response(error_response(403, "Forbidden"));
    }
    let mut extension_offers = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("sec-websocket-extensions"));
    if extension_offers.next().is_some_and(|header| {
        std::str::from_utf8(header.value)
            .map(str::trim)
            .map_or(true, str::is_empty)
    }) || extension_offers.next().is_some()
    {
        return Parsed::Response(error_response(400, "Bad Request"));
    }
    let Some(connection) = header_text(headers, "connection") else {
        return Parsed::Response(error_response(400, "Bad Request"));
    };
    let Some(upgrade) = header_text(headers, "upgrade") else {
        return Parsed::Response(error_response(400, "Bad Request"));
    };
    if !comma_tokens(connection).any(|token| token.eq_ignore_ascii_case("upgrade"))
        || !upgrade.eq_ignore_ascii_case("websocket")
        || header_text(headers, "sec-websocket-version") != Some("13")
    {
        return Parsed::Response(error_response(400, "Bad Request"));
    }
    let Some(key) = header_text(headers, "sec-websocket-key") else {
        return Parsed::Response(error_response(400, "Bad Request"));
    };
    let Some(accept) = accept_key(key) else {
        return Parsed::Response(error_response(400, "Bad Request"));
    };
    let Some(protocols) = header_text(headers, "sec-websocket-protocol") else {
        return Parsed::Response(error_response(403, "Forbidden"));
    };
    let offered: Vec<_> = protocols.split(',').map(str::trim).collect();
    if offered.len() != 2
        || offered.iter().any(|item| item.is_empty())
        || offered
            .iter()
            .filter(|item| **item == route.protocol())
            .count()
            != 1
    {
        return Parsed::Response(error_response(403, "Forbidden"));
    }
    let Some(csrf) = offered
        .iter()
        .find_map(|item| item.strip_prefix(CSRF_PROTOCOL_PREFIX))
        .filter(|value| valid_nonce(value))
    else {
        return Parsed::Response(error_response(403, "Forbidden"));
    };
    let Some(cookie) = header_text(headers, "cookie").and_then(session_cookie) else {
        return Parsed::Response(error_response(403, "Forbidden"));
    };
    let authorizer = sessions.authorizer();
    let session = match route {
        SocketRoute::Stream => authorizer
            .authorize_stream(host, origin, cookie, csrf)
            .map(SocketSession::Stream),
        SocketRoute::Terminal => authorizer
            .authorize_terminal(host, origin, cookie, csrf)
            .map(SocketSession::Terminal),
    };
    let Some(session) = session else {
        return Parsed::Response(error_response(403, "Forbidden"));
    };
    Parsed::Upgrade { accept, session }
}

fn header_text<'a>(headers: &'a [httparse::Header<'a>], name: &str) -> Option<&'a str> {
    one_header(headers, name).and_then(|value| std::str::from_utf8(value).ok())
}

fn comma_tokens(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

fn valid_nonce(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn session_cookie(header: &str) -> Option<&str> {
    let mut found = None;
    for item in header.split(';').map(str::trim) {
        let Some((name, value)) = item.split_once('=') else {
            continue;
        };
        if name == SESSION_COOKIE_NAME {
            if found.is_some() || !valid_nonce(value) {
                return None;
            }
            found = Some(value);
        }
    }
    found
}

fn serve_websocket(
    stream: &mut TcpStream,
    session: &StreamSession,
    buffered: &[u8],
    streams: &FloorStreams,
    clock: &(dyn Clock + Send + Sync),
    phase: &AtomicU8,
    handle: &Handle,
) -> io::Result<()> {
    stream.set_read_timeout(Some(ACCEPT_POLL))?;
    let mut decoder = Decoder::default();
    let mut subscription = None;
    if !buffered.is_empty() {
        let messages = decoder
            .push(buffered)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if !handle_messages(
            stream,
            messages,
            streams,
            session,
            &mut subscription,
            clock,
            phase,
            handle,
        )? {
            return Ok(());
        }
    }

    let mut input = [0_u8; 8192];
    loop {
        if stop_requested(phase) || !session.is_valid() {
            let _ = write_bytes(stream, &close_frame(), phase);
            return Ok(());
        }
        match stream.read(&mut input) {
            Ok(0) => return Ok(()),
            Ok(read) => {
                let messages = decoder
                    .push(&input[..read])
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if !handle_messages(
                    stream,
                    messages,
                    streams,
                    session,
                    &mut subscription,
                    clock,
                    phase,
                    handle,
                )? {
                    return Ok(());
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error),
        }

        if let Some(active) = &subscription {
            streams.before_output_drain(active);
            loop {
                match active.next(Duration::ZERO) {
                    QueueRead::Frame(frame) => {
                        if !session.is_valid() {
                            let _ = write_bytes(stream, &close_frame(), phase);
                            return Ok(());
                        }
                        write_activity_frame(stream, &frame, clock, phase)?;
                    }
                    QueueRead::Empty => break,
                    QueueRead::Overflowed | QueueRead::Ended => {
                        let _ = write_bytes(stream, &close_frame(), phase);
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the authenticated stream handler shares its clock and admission inputs across subscribe and output messages"
)]
fn handle_messages(
    stream: &mut TcpStream,
    messages: Vec<Message>,
    streams: &FloorStreams,
    session: &StreamSession,
    subscription: &mut Option<crate::floor_stream::Subscription>,
    clock: &(dyn Clock + Send + Sync),
    phase: &AtomicU8,
    handle: &Handle,
) -> io::Result<bool> {
    for message in messages {
        match message {
            Message::Text(text) => {
                let client = decode_client(&text)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                match (subscription.is_some(), client) {
                    (false, ClientMessage::Subscribe {}) => {
                        if !session.touch_subscribe() {
                            return Ok(false);
                        }
                        let captured = handle
                            .floor_subscribe(session.identity())
                            .map_err(io::Error::other)?;
                        let opened = streams
                            .finish_subscription(captured)
                            .map_err(|error| io::Error::other(error.to_string()))?;
                        if !session.is_valid() {
                            return Ok(false);
                        }
                        write_activity_frame(stream, &opened.snapshot, clock, phase)?;
                        *subscription = Some(opened.subscription);
                    }
                    (true, ClientMessage::Unsubscribe {}) => {
                        let _ = write_bytes(stream, &close_frame(), phase);
                        return Ok(false);
                    }
                    (true, ClientMessage::OutputSelect { agent_id, view }) => {
                        if !session.is_valid() {
                            return Ok(false);
                        }
                        streams
                            .select_output(
                                subscription.as_ref().expect("active subscription"),
                                agent_id,
                                view,
                            )
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    }
                    (true, ClientMessage::OutputClear {}) => {
                        if !session.is_valid() {
                            return Ok(false);
                        }
                        streams
                            .clear_output(subscription.as_ref().expect("active subscription"))
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    }
                    _ => return Ok(false),
                }
            }
            Message::Ping(bytes) => {
                if !session.is_valid() {
                    return Ok(false);
                }
                let pong = pong_frame(&bytes).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid WebSocket ping")
                })?;
                write_bytes(stream, &pong, phase)?;
            }
            Message::Pong(_) => {}
            Message::Close => {
                let _ = write_bytes(stream, &close_frame(), phase);
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn write_upgrade(
    stream: &mut TcpStream,
    accept: &str,
    protocol: &str,
    phase: &AtomicU8,
) -> io::Result<()> {
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: {protocol}\r\n\r\n"
    );
    write_bytes(stream, response.as_bytes(), phase)
}

pub(crate) fn write_bytes(
    stream: &mut TcpStream,
    bytes: &[u8],
    phase: &AtomicU8,
) -> io::Result<()> {
    let deadline = Instant::now() + RESPONSE_WRITE_DEADLINE;
    let mut written = 0;
    while written < bytes.len() {
        if stop_requested(phase) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Floor listener is stopping",
            ));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Floor response deadline",
            ));
        }
        stream.set_write_timeout(Some(deadline.saturating_duration_since(now)))?;
        written += stream.write(&bytes[written..])?;
    }
    stream.flush()
}

fn write_activity_frame(
    stream: &mut TcpStream,
    frame: &[u8],
    clock: &(dyn Clock + Send + Sync),
    phase: &AtomicU8,
) -> io::Result<()> {
    if !crate::floor_stream::frame_has_live_activity(frame) {
        return write_bytes(stream, frame, phase);
    }
    let fresh = crate::floor_stream::refresh_activity_frame(frame, clock.now())
        .map_err(io::Error::other)?;
    write_bytes(stream, fresh.as_ref(), phase)
}

fn valid_static_framing(version: Option<u8>, headers: &[httparse::Header<'_>]) -> bool {
    if !matches!(version, Some(0 | 1)) {
        return false;
    }
    let hosts = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("host"))
        .count();
    if version == Some(1) && hosts != 1 {
        return false;
    }
    if forbidden_framing(headers) {
        return false;
    }
    let lengths: Vec<_> = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("content-length"))
        .collect();
    lengths.is_empty() || (lengths.len() == 1 && lengths[0].value == b"0")
}

fn forbidden_framing(headers: &[httparse::Header<'_>]) -> bool {
    headers.iter().any(|header| {
        header.name.eq_ignore_ascii_case("transfer-encoding")
            || header.name.eq_ignore_ascii_case("expect")
            || header.name.eq_ignore_ascii_case("upgrade")
    })
}

fn one_header<'a>(headers: &'a [httparse::Header<'a>], name: &str) -> Option<&'a [u8]> {
    let mut matches = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case(name));
    let value = matches.next()?.value;
    matches.next().is_none().then_some(value)
}

fn unique_header<'a>(
    headers: &'a [httparse::Header<'a>],
    name: &str,
) -> Result<Option<&'a [u8]>, ()> {
    let mut matches = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case(name));
    let value = matches.next().map(|header| header.value);
    if matches.next().is_some() {
        Err(())
    } else {
        Ok(value)
    }
}

fn write_response(
    stream: &mut TcpStream,
    response: &Response<'_>,
    csp: &str,
    phase: &AtomicU8,
) -> io::Result<()> {
    let mut headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: {}\r\nContent-Security-Policy: {}\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nCross-Origin-Resource-Policy: same-origin\r\n",
        response.code,
        response.reason,
        response.body.len(),
        response.cache,
        csp,
    );
    if let Some(content_type) = response.content_type {
        use std::fmt::Write as _;
        let _ = write!(&mut headers, "Content-Type: {content_type}\r\n");
    }
    if let Some(allow) = response.allow {
        use std::fmt::Write as _;
        let _ = write!(&mut headers, "Allow: {allow}\r\n");
    }
    if let Some(cookie) = &response.set_cookie {
        use std::fmt::Write as _;
        let _ = write!(&mut headers, "Set-Cookie: {cookie}\r\n");
    }
    headers.push_str("\r\n");
    let mut output = headers.into_bytes();
    output.extend_from_slice(response.body.as_ref());
    let deadline = Instant::now() + RESPONSE_WRITE_DEADLINE;
    let mut written = 0;
    while written < output.len() {
        if stop_requested(phase) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Floor listener is stopping",
            ));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Floor response deadline",
            ));
        }
        stream.set_write_timeout(Some(deadline.saturating_duration_since(now)))?;
        written += stream.write(&output[written..])?;
    }
    stream.flush()
}

fn discard_request_remainder(stream: &mut impl RequestReader, phase: &AtomicU8) {
    let deadline = Instant::now() + REQUEST_DRAIN_DEADLINE;
    let mut remaining = MAX_REQUEST_DRAIN_BYTES;
    let mut chunk = [0_u8; 1024];
    while remaining != 0 {
        if stop_requested(phase) {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let timeout = deadline.saturating_duration_since(now).min(ACCEPT_POLL);
        if stream.set_request_timeout(Some(timeout)).is_err() {
            return;
        }
        let room = remaining.min(chunk.len());
        match stream.read(&mut chunk[..room]) {
            Ok(0) => return,
            Ok(read) => remaining -= read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return,
        }
    }
}

fn error_response(code: u16, reason: &'static str) -> Response<'static> {
    Response {
        code,
        reason,
        body: Cow::Borrowed(&[]),
        content_type: None,
        cache: "no-store",
        allow: None,
        set_cookie: None,
    }
}

fn find_head_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| at + 4)
}

fn request_line_len(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|window| window == b"\r\n")
}

fn strict_crlf(bytes: &[u8]) -> bool {
    bytes.iter().enumerate().all(|(index, byte)| match byte {
        b'\n' => index > 0 && bytes[index - 1] == b'\r',
        b'\r' => bytes.get(index + 1) == Some(&b'\n'),
        _ => true,
    })
}

/// Validate and retain one release bundle without opening a TCP listener.
#[expect(
    clippy::too_many_lines,
    reason = "one pass keeps manifest validation, byte retention and tree verification atomic"
)]
pub fn prepare(settings: &FloorSettings) -> Result<PreparedAssets> {
    ensure!(
        settings.assets_dir.is_absolute(),
        "Floor asset root is not absolute"
    );
    let root = settings.assets_dir.canonicalize().with_context(|| {
        format!(
            "resolving Floor asset root {}",
            settings.assets_dir.display()
        )
    })?;
    ensure!(root.is_dir(), "Floor asset root is not a directory");

    let manifest_path = root.join(MANIFEST_NAME);
    let manifest_metadata = fs::symlink_metadata(&manifest_path)
        .with_context(|| format!("reading {MANIFEST_NAME} metadata"))?;
    ensure!(
        !link_or_reparse(&manifest_metadata) && manifest_metadata.is_file(),
        "release manifest is not a regular unlinked file"
    );
    let mut manifest_file = File::open(&manifest_path).context("opening release manifest")?;
    let manifest_size = manifest_file
        .metadata()
        .context("reading opened release manifest metadata")?
        .len();
    ensure!(
        manifest_size <= MAX_MANIFEST_BYTES,
        "release manifest exceeds {MAX_MANIFEST_BYTES} bytes"
    );
    let manifest_bytes = read_bounded(&mut manifest_file, MAX_MANIFEST_BYTES, "release manifest")
        .with_context(|| format!("reading {MANIFEST_NAME}"))?;
    let manifest: ReleaseManifest =
        serde_json::from_slice(&manifest_bytes).context("parsing release manifest")?;

    ensure!(
        manifest.schema_version == 1,
        "unsupported release manifest schema"
    );
    ensure!(
        manifest.base_path == BASE_PATH,
        "release manifest base path mismatch"
    );
    ensure!(
        manifest.daemon_version == env!("CARGO_PKG_VERSION"),
        "release manifest daemon version mismatch"
    );
    ensure!(
        manifest.browser_wire_version == BROWSER_WIRE_VERSION,
        "release manifest browser wire version mismatch"
    );
    ensure!(
        manifest.files.len() <= MAX_ASSETS,
        "release manifest exceeds {MAX_ASSETS} files"
    );

    let mut seen = BTreeSet::new();
    let mut total = 0_u64;
    let mut assets = BTreeMap::new();
    let mut tree_rows = Vec::with_capacity(manifest.files.len());
    let mut index = false;

    for row in &manifest.files {
        let segments = manifest_segments(&row.path)?;
        ensure!(
            seen.insert(row.path.clone()),
            "duplicate manifest path {}",
            row.path
        );
        validate_policy(row)?;
        let remaining = MAX_ASSET_BYTES
            .checked_sub(total)
            .context("asset catalog byte accounting exceeded its bound")?;
        ensure!(
            row.bytes <= remaining,
            "declared asset exceeds remaining catalog bytes for {}",
            row.path
        );
        let candidate = checked_file(&root, &segments)?;
        let mut file = File::open(&candidate)
            .with_context(|| format!("opening manifest path {}", row.path))?;
        let opened = file
            .metadata()
            .with_context(|| format!("reading opened metadata for {}", row.path))?;
        ensure!(
            opened.is_file(),
            "manifest path is not a regular opened file"
        );
        ensure!(
            opened.len() <= row.bytes,
            "asset exceeds declared bytes for {}",
            row.path
        );
        let bytes = read_bounded(&mut file, row.bytes, "asset")
            .with_context(|| format!("reading manifest path {}", row.path))?;
        let actual_len = u64::try_from(bytes.len()).context("asset length exceeds u64")?;
        ensure!(
            actual_len == row.bytes,
            "asset size mismatch for {}",
            row.path
        );
        let digest = sha256_hex(&bytes);
        ensure!(
            digest == row.sha256,
            "asset SHA-256 mismatch for {}",
            row.path
        );
        total = total
            .checked_add(row.bytes)
            .context("asset byte total overflow")?;
        ensure!(
            total <= MAX_ASSET_BYTES,
            "asset catalog exceeds {MAX_ASSET_BYTES} bytes"
        );

        index |= row.path == "index.html";
        tree_rows.push((row.path.as_str(), row.bytes, row.sha256.as_str()));
        let route = if row.path == "index.html" {
            BASE_PATH.to_owned()
        } else {
            format!("{BASE_PATH}{}", row.path)
        };
        assets.insert(
            route,
            Asset {
                bytes,
                content_type: content_type(&row.path)?,
                cache: match row.cache {
                    CachePolicy::NoStore => "no-store",
                    CachePolicy::Immutable => "public, max-age=31536000, immutable",
                    CachePolicy::Unknown => unreachable!("validated above"),
                },
            },
        );
    }

    ensure!(index, "release manifest has no index.html");
    tree_rows.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut tree = Sha256::new();
    for (path, bytes, digest) in tree_rows {
        tree.update(path.as_bytes());
        tree.update([0]);
        tree.update(bytes.to_string().as_bytes());
        tree.update([0]);
        tree.update(digest.as_bytes());
        tree.update(b"\n");
    }
    ensure!(
        lower_hex(&tree.finalize()) == manifest.asset_tree_sha256,
        "release manifest asset tree SHA-256 mismatch"
    );

    Ok(PreparedAssets {
        assets,
        manifest: manifest_bytes,
        port: settings.port,
        remembered: None,
        answer: false,
        control: FloorControl::Off,
        recovery: false,
        terminal: false,
        git: None,
        diff: None,
        board: None,
    })
}

fn validate_policy(row: &ManifestFile) -> Result<()> {
    ensure!(
        is_lower_sha256(&row.sha256),
        "invalid SHA-256 for {}",
        row.path
    );
    if row.path == "index.html" {
        ensure!(
            row.cache == CachePolicy::NoStore,
            "index.html must be no-store"
        );
    } else {
        ensure!(
            row.path.starts_with("assets/"),
            "immutable manifest path is outside assets/"
        );
        ensure!(
            row.cache == CachePolicy::Immutable,
            "immutable asset has wrong cache policy"
        );
        ensure!(
            hashed_asset_path(&row.path),
            "immutable asset path is not content-hashed"
        );
    }
    Ok(())
}

fn manifest_segments(path: &str) -> Result<Vec<&str>> {
    if path.is_empty()
        || !path.is_ascii()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains(['\\', ':', '%'])
    {
        bail!("invalid manifest path {path:?}");
    }
    let segments: Vec<_> = path.split('/').collect();
    if segments.iter().any(|segment| {
        segment.is_empty()
            || *segment == "."
            || *segment == ".."
            || !segment
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    }) {
        bail!("invalid manifest path {path:?}");
    }
    Ok(segments)
}

fn checked_file(root: &Path, segments: &[&str]) -> Result<PathBuf> {
    let mut candidate = root.to_path_buf();
    for segment in segments {
        candidate.push(segment);
        let metadata = fs::symlink_metadata(&candidate)
            .with_context(|| format!("reading manifest path {}", candidate.display()))?;
        ensure!(
            !link_or_reparse(&metadata),
            "manifest path contains a link or reparse point"
        );
    }
    ensure!(
        candidate
            .components()
            .all(|part| !matches!(part, Component::ParentDir)),
        "manifest path contains a parent component"
    );
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("resolving manifest path {}", candidate.display()))?;
    ensure!(
        canonical.starts_with(root),
        "manifest path escapes asset root"
    );
    ensure!(canonical.is_file(), "manifest path is not a regular file");
    Ok(canonical)
}

#[cfg(unix)]
fn link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Whether `path` carries the approved hyphen-delimited Vite hash suffix.
#[doc(hidden)]
#[must_use]
pub fn hashed_asset_path(path: &str) -> bool {
    if !path.starts_with("assets/") {
        return false;
    }
    let file = path.rsplit('/').next().unwrap_or_default();
    let Some((stem, extension)) = file.rsplit_once('.') else {
        return false;
    };
    !extension.is_empty()
        && stem.match_indices('-').any(|(delimiter, _)| {
            let suffix = &stem[delimiter + 1..];
            delimiter > 0
                && suffix.len() >= 8
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
}

/// The finite served-extension table shared with the static build checks.
#[doc(hidden)]
#[must_use]
pub fn served_content_type(path: &str) -> Option<&'static str> {
    if path == "index.html" {
        return Some("text/html; charset=utf-8");
    }
    match Path::new(path).extension().and_then(|value| value.to_str()) {
        Some("js") => Some("text/javascript; charset=utf-8"),
        Some("css") => Some("text/css; charset=utf-8"),
        Some("json") => Some("application/json"),
        Some("png") => Some("image/png"),
        Some("svg") => Some("image/svg+xml"),
        Some("woff2") => Some("font/woff2"),
        _ => None,
    }
}

fn content_type(path: &str) -> Result<&'static str> {
    served_content_type(path)
        .ok_or_else(|| anyhow::anyhow!("manifest path has an unsupported content type: {path}"))
}

fn read_bounded(reader: &mut impl Read, limit: u64, label: &str) -> Result<Vec<u8>> {
    let read_limit = limit
        .checked_add(1)
        .context("bounded read limit overflow")?;
    let mut bytes = Vec::new();
    reader
        .take(read_limit)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading bounded {label}"))?;
    ensure!(
        u64::try_from(bytes.len()).context("bounded read length exceeds u64")? <= limit,
        "{label} exceeds {limit} bytes"
    );
    Ok(bytes)
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_hex(bytes: &[u8]) -> String {
    lower_hex(&Sha256::digest(bytes))
}

fn lower_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions() -> FloorSessions {
        FloorSessions::new(
            "127.0.0.1:43123".parse().expect("loopback address"),
            Arc::new(dudes_supervisor::testing::FakeClock::frozen(0)),
        )
    }

    fn session_credentials(sessions: &FloorSessions) -> (String, String) {
        let issued = sessions.issue().expect("one bootstrap");
        let exchange = sessions
            .exchange(
                "127.0.0.1:43123",
                "http://127.0.0.1:43123",
                issued.bootstrap.as_bytes(),
            )
            .expect("one live session");
        (exchange.cookie, exchange.csrf)
    }

    #[test]
    fn refused_request_discard_never_reads_past_65536_bytes() {
        let mut input = RequestInput {
            bytes: io::Cursor::new(vec![b'x'; 65_537]),
            reads: 0,
        };
        discard_request_remainder(&mut input, &AtomicU8::new(READY));
        assert_eq!(input.bytes.position(), 65_536);
    }

    #[test]
    fn refused_request_discard_observes_stop_before_reading() {
        let mut input = RequestInput {
            bytes: io::Cursor::new(b"discard".to_vec()),
            reads: 0,
        };
        discard_request_remainder(&mut input, &AtomicU8::new(STOP_REQUESTED));
        assert_eq!(input.reads, 0);
        assert_eq!(input.bytes.position(), 0);
        discard_request_remainder(&mut input, &AtomicU8::new(READY));
        assert_eq!(input.bytes.position(), 7);
    }

    struct TrickleInput {
        remaining: usize,
        reads: usize,
        timeout: std::cell::Cell<Duration>,
    }

    impl Read for TrickleInput {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(10).min(self.timeout.get()));
            bytes[0] = b'x';
            self.reads += 1;
            self.remaining -= 1;
            Ok(1)
        }
    }

    impl RequestReader for TrickleInput {
        fn set_request_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.timeout.set(timeout.expect("bounded discard read"));
            Ok(())
        }
    }

    #[test]
    fn refused_request_discard_does_not_renew_deadline_after_progress() {
        let mut input = TrickleInput {
            remaining: 40,
            reads: 0,
            timeout: std::cell::Cell::new(Duration::from_millis(25)),
        };
        discard_request_remainder(&mut input, &AtomicU8::new(READY));
        assert!(
            (1..=21).contains(&input.reads),
            "discard read {} trickle bytes",
            input.reads
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    struct CapturedResponse {
        code: u16,
        reason: &'static str,
        body: Vec<u8>,
        content_type: Option<&'static str>,
        cache: &'static str,
        allow: Option<&'static str>,
        set_cookie: Option<String>,
    }

    /// The catalog these unit tests serve: no answer route, as in the default configuration.
    const fn catalog<'a>(assets: &'a BTreeMap<String, Asset>, manifest: &'a [u8]) -> Catalog<'a> {
        static MEMORY: MemoryReads = MemoryReads::new();
        Catalog {
            assets,
            manifest,
            answer: false,
            control: FloorControl::Off,
            recovery: false,
            terminal: false,
            git: None,
            diff: None,
            board: None,
            memory: &MEMORY,
        }
    }

    fn response_for(request: Vec<u8>, sessions: &FloorSessions) -> CapturedResponse {
        let mut input = RequestInput {
            bytes: io::Cursor::new(request),
            reads: 0,
        };
        let assets = BTreeMap::new();
        let phase = AtomicU8::new(READY);
        let action = read_request(
            &mut input,
            catalog(&assets, &[]),
            sessions,
            &phase,
            &Handle::new(std::sync::mpsc::channel().0),
        )
        .expect("request parse")
        .expect("one response");
        let ConnectionAction::Response(response) = action else {
            panic!("the request unexpectedly upgraded");
        };
        CapturedResponse {
            code: response.code,
            reason: response.reason,
            body: response.body.into_owned(),
            content_type: response.content_type,
            cache: response.cache,
            allow: response.allow,
            set_cookie: response.set_cookie,
        }
    }

    fn recover_head(cookie: &str) -> String {
        format!(
            "POST /floor/session/recover HTTP/1.1\r\nHost: 127.0.0.1:43123\r\nOrigin: http://127.0.0.1:43123\r\nCookie: {SESSION_COOKIE_NAME}={cookie}\r\nContent-Length: 0\r\n\r\n"
        )
    }

    fn upgrade_head(cookie: &str, csrf: &str) -> Vec<u8> {
        format!(
            "GET {STREAM_PATH} HTTP/1.1\r\nHost: 127.0.0.1:43123\r\nOrigin: http://127.0.0.1:43123\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: {FLOOR_PROTOCOL}, {CSRF_PROTOCOL_PREFIX}{csrf}\r\nCookie: {SESSION_COOKIE_NAME}={cookie}\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn recover_route_is_exact_http11_empty_post() {
        let sessions = sessions();
        let (cookie, _) = session_credentials(&sessions);
        assert_eq!(
            response_for(recover_head(&cookie).into_bytes(), &sessions).code,
            200
        );

        for malformed in [
            recover_head(&cookie).replace("HTTP/1.1", "HTTP/1.0"),
            recover_head(&cookie).replace("Content-Length: 0\r\n", ""),
            recover_head(&cookie).replace("Content-Length: 0", "Content-Length: 00"),
            recover_head(&cookie).replace(
                "Content-Length: 0\r\n",
                "Content-Length: 0\r\nTransfer-Encoding: chunked\r\n",
            ),
            recover_head(&cookie).replace(
                "Content-Length: 0\r\n",
                "Content-Length: 0\r\nExpect: 100-continue\r\n",
            ),
            recover_head(&cookie).replace("\r\n\r\n", "\r\n\r\nx"),
            recover_head(&cookie).replace(
                "Host: 127.0.0.1:43123\r\n",
                "Host: 127.0.0.1:43123\r\nHost: 127.0.0.1:43123\r\n",
            ),
            recover_head(&cookie).replace(
                "Origin: http://127.0.0.1:43123\r\n",
                "Origin: http://127.0.0.1:43123\r\nOrigin: http://127.0.0.1:43123\r\n",
            ),
            recover_head(&cookie).replace(
                "Cookie: ",
                &format!("Cookie: {SESSION_COOKIE_NAME}={cookie}\r\nCookie: "),
            ),
        ] {
            assert_eq!(
                response_for(malformed.into_bytes(), &sessions).code,
                400,
                "malformed recovery framing must be 400"
            );
        }

        for other in [
            recover_head(&cookie).replace("/floor/session/recover", "/floor/session/recover/"),
            recover_head(&cookie).replace("/floor/session/recover", "/floor/session/recover?x=1"),
            recover_head(&cookie).replace("/floor/session/recover", "/floor/recover"),
            recover_head(&cookie).replace("POST", "GET"),
        ] {
            let code = response_for(other.into_bytes(), &sessions).code;
            assert!(!(200..400).contains(&code), "route alias returned {code}");
        }

        let long_path = format!(
            "POST /{} HTTP/1.1\r\nHost: 127.0.0.1:43123\r\n\r\n",
            "x".repeat(MAX_REQUEST_LINE)
        );
        assert_eq!(response_for(long_path.into_bytes(), &sessions).code, 414);

        let mut too_many = recover_head(&cookie);
        too_many = too_many.replace(
            "\r\n\r\n",
            &format!("\r\n{}\r\n", "X-Bound: one\r\n".repeat(MAX_HEADERS)),
        );
        assert_eq!(response_for(too_many.into_bytes(), &sessions).code, 431);

        let oversized = format!(
            "POST {RECOVERY_PATH} HTTP/1.1\r\nHost: 127.0.0.1:43123\r\nX-Fill: {}",
            "x".repeat(MAX_REQUEST_HEAD)
        );
        assert_eq!(response_for(oversized.into_bytes(), &sessions).code, 431);
    }

    #[test]
    fn recover_cookie_only_requires_exact_origin() {
        let sessions = sessions();
        let (cookie, old_csrf) = session_credentials(&sessions);
        let refusal = CapturedResponse {
            code: 403,
            reason: "Forbidden",
            body: Vec::new(),
            content_type: None,
            cache: "no-store",
            allow: None,
            set_cookie: None,
        };
        for refused in [
            recover_head(&cookie).replace("Host: 127.0.0.1:43123\r\n", ""),
            recover_head(&cookie).replace("127.0.0.1:43123", "localhost.invalid:43123"),
            recover_head(&cookie).replace("Origin: http://127.0.0.1:43123\r\n", ""),
            recover_head(&cookie).replace(
                "Origin: http://127.0.0.1:43123",
                "Origin: http://localhost.invalid:43123",
            ),
            recover_head(&cookie)
                .replace(&format!("Cookie: {SESSION_COOKIE_NAME}={cookie}\r\n"), ""),
            recover_head(&cookie).replace(&cookie, &"A".repeat(43)),
        ] {
            assert_eq!(response_for(refused.into_bytes(), &sessions), refusal);
        }

        let success = response_for(recover_head(&cookie).into_bytes(), &sessions);
        assert_eq!(success.code, 200);
        assert_eq!(success.reason, "OK");
        assert_eq!(success.content_type, Some("application/json"));
        assert_eq!(success.cache, "no-store");
        assert_eq!(success.allow, None);
        assert_eq!(success.set_cookie, None);
        let value: serde_json::Value =
            serde_json::from_slice(&success.body).expect("one recovery JSON object");
        assert_eq!(value.as_object().map(serde_json::Map::len), Some(1));
        let csrf = value["csrf"].as_str().expect("one CSRF string");
        assert!(valid_nonce(csrf));
        assert_eq!(success.body, format!(r#"{{"csrf":"{csrf}"}}"#).as_bytes());

        assert_eq!(
            response_for(recover_head(&cookie).into_bytes(), &sessions),
            refusal,
            "the pending window returns the same refusal"
        );
        let assets = BTreeMap::new();
        assert!(matches!(
            parse_request(&upgrade_head(&cookie, &old_csrf), catalog(&assets, &[]), &sessions),
            Parsed::Response(response) if response.code == 403
        ));
        assert!(matches!(
            parse_request(
                &upgrade_head(&cookie, csrf),
                catalog(&assets, &[]),
                &sessions
            ),
            Parsed::Upgrade { .. }
        ));

        let next = response_for(recover_head(&cookie).into_bytes(), &sessions);
        let next_value: serde_json::Value =
            serde_json::from_slice(&next.body).expect("second recovery JSON");
        let next_csrf = next_value["csrf"].as_str().expect("second CSRF");
        let held = parse_request(
            &upgrade_head(&cookie, next_csrf),
            catalog(&assets, &[]),
            &sessions,
        );
        assert!(matches!(&held, Parsed::Upgrade { .. }));
        assert_eq!(
            response_for(recover_head(&cookie).into_bytes(), &sessions),
            refusal,
            "an open socket returns the same refusal"
        );
    }

    struct ScriptedReader {
        read: usize,
    }

    impl Read for ScriptedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            buffer.fill(b'x');
            self.read += buffer.len();
            Ok(buffer.len())
        }
    }

    #[test]
    fn bounded_reader_stops_at_limit_plus_one() {
        let mut reader = ScriptedReader { read: 0 };
        let error = read_bounded(&mut reader, 4, "scripted")
            .expect_err("limit plus one is a bounded refusal");
        assert!(format!("{error:#}").contains("scripted exceeds 4 bytes"));
        assert_eq!(reader.read, 5, "the helper consumed only limit plus one");
    }

    struct TimeoutFailure {
        sets: AtomicUsize,
        reads: AtomicUsize,
    }

    impl Read for TimeoutFailure {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        }
    }

    impl RequestReader for TimeoutFailure {
        fn set_request_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            self.sets.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("injected timeout setup failure"))
        }
    }

    struct RequestInput {
        bytes: io::Cursor<Vec<u8>>,
        reads: usize,
    }

    impl Read for RequestInput {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            self.bytes.read(buffer)
        }
    }

    impl RequestReader for RequestInput {
        fn set_request_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn read_timeout_setup_failure_propagates_without_read_or_response() {
        let mut stream = TimeoutFailure {
            sets: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
        };
        let assets = BTreeMap::new();
        let phase = AtomicU8::new(READY);
        let Err(error) = read_request(
            &mut stream,
            catalog(&assets, &[]),
            &sessions(),
            &phase,
            &Handle::new(std::sync::mpsc::channel().0),
        ) else {
            panic!("timeout setup error returned a response instead of propagating");
        };
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "injected timeout setup failure");
        assert_eq!(stream.sets.load(Ordering::SeqCst), 1);
        assert_eq!(
            stream.reads.load(Ordering::SeqCst),
            0,
            "timeout setup failure closes before any blocking read"
        );

        let mut valid = RequestInput {
            bytes: io::Cursor::new(
                b"GET /floor/release-manifest.json HTTP/1.1\r\nHost: floor\r\n\r\n".to_vec(),
            ),
            reads: 0,
        };
        let response = read_request(
            &mut valid,
            catalog(&assets, b"manifest"),
            &sessions(),
            &phase,
            &Handle::new(std::sync::mpsc::channel().0),
        )
        .expect("a following request has no connection-local error")
        .expect("a following valid request returns a response");
        let ConnectionAction::Response(response) = response else {
            panic!("a static request cannot upgrade");
        };
        assert_eq!(response.code, 200);
        assert_eq!(response.body.as_ref(), b"manifest");
        assert_eq!(valid.reads, 1);
    }
}
