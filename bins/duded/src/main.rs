//! `duded` — the Dudes daemon.
//!
//! # This is the only file in the package that names a concrete child owner
//!
//! Everything under `src/` but this file is generic over [`ProcessHost`] and over
//! [`PredicateRunner`]. That is not tidiness: the code alone cannot establish that the daemon
//! reaches the host through the trait — a daemon holding a concrete `RealHost` and calling the inherent method would
//! satisfy invariant 10 while making the trait change pointless. So the concrete types are
//! constructed here, handed in as seams, and `tests/host_seam.rs` is the scan that keeps every
//! other file free of them.
//!
//! # Everything ambient is read here, once
//!
//! The environment, the process's own signal mask, the argv. `duded::run` reads none of them, which
//! is what makes the whole daemon drivable from a test with no process-wide mutation.
//!
//! # This file does not choose an authorizer, and there is no longer one to choose
//!
//! The daemon runs `PeerClassifier` — the peer-classification table — and `duded::boot` builds it, because
//! its two inputs come into existence inside `boot`: the operator credential is minted at startup
//! step 4 and the token projection is the fold step 3 replayed. Neither can be handed in from
//! here, so neither is.
//!
//! What this file passes is the one input that *is* a choice: `Seams.resolver`, the peer-classification column.
//! Linux, macOS and Windows select the host-owned process-ancestry relation. Unsupported targets select
//! `classify::NoWalk`, which walks nothing and answers `Unresolvable` for every peer.
//!
//! The two permissive authorisers this workspace contains — `AdmitAll` and `RefuseAll` — live
//! behind `dudes-rpc`'s `testing` feature, and this package does not enable it. So the daemon
//! **cannot link one at all**: that is the compiler enforcing it, not this comment.
//!
//! # No subscriber is installed
//!
//! `tracing` events are emitted; where they go is the operator's build and the packaging. A
//! daemon that installed one would be choosing a log format on the operator's behalf, and adding a
//! subscriber crate is a dependency decision rather than a wiring one.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use duded::config::{ProviderTables, load_operator_and_memory_with_options};
use duded::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner, allowlisted};
use duded::settings::{DEFAULT_BEAT, DEFAULT_MAX_CONNECTIONS, SOCKET_NAME, Seams, Settings};
use duded::startup::Witness;
use dudes_host::{CapturedOutcome, OneShot, OneShotOutcome, RealHost};
use dudes_journal::JournalOptions;
use dudes_policy::BreakerConfig;
use dudes_proto::ExitStatus;
use dudes_proto::rpc::pty::PtyId;
use dudes_providers::adapter::Adapter;
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::codex::CodexAdapter;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_providers::pi::PiAdapter;
use dudes_providers::registry::Adapters;
use dudes_supervisor::host::{AdoptionKey, Clock, Rng};
use dudes_types::{Escaped, Timestamp};

/// The sole adapter from the daemon's neutral port to the concrete PTY owner.
struct MainPtyHost {
    host: dudes_pty::PtyHost,
    drains: BTreeMap<PtyId, Drain>,
    native_closes: BTreeMap<PtyId, NativePtyClose>,
    observed: Option<Arc<Mutex<BTreeSet<PtyId>>>>,
}

struct NativePtyClose {
    token: dudes_supervisor::host::NativeBoundaryReceipt,
    identity: dudes_pty::PtyCloseIdentity,
    receipt: Option<dudes_pty::PtyCloseReceipt>,
}

/// Reads continuously, because a PTY nobody reads blocks its child's writes.
struct Drain {
    output: duded::pty_output::PtyOutput,
    thread: std::thread::JoinHandle<()>,
}

impl Drain {
    fn start(pty: PtyId, mut reader: dudes_pty::PtyReader) -> io::Result<Self> {
        let (sink, output) = duded::pty_output::channel();
        let thread = std::thread::Builder::new()
            .name(format!("duded-pty-output-{}", pty.0))
            .spawn(move || {
                while let Ok(dudes_pty::PtyRead::Data(bytes)) = reader.read() {
                    sink.push(&bytes);
                }
            })?;
        Ok(Self { output, thread })
    }

    /// Only after the host confirmed the close, which ended the reader this thread drains.
    fn finish_closed(self) {
        if self.thread.join().is_err() {
            tracing::error!("a PTY output drain panicked");
        }
    }
}

impl MainPtyHost {
    const fn new() -> Self {
        Self {
            host: dudes_pty::PtyHost::new(),
            drains: BTreeMap::new(),
            native_closes: BTreeMap::new(),
            observed: None,
        }
    }

    #[cfg(all(test, target_os = "macos"))]
    fn observed(observed: Arc<Mutex<BTreeSet<PtyId>>>) -> Self {
        Self {
            host: dudes_pty::PtyHost::new(),
            drains: BTreeMap::new(),
            native_closes: BTreeMap::new(),
            observed: Some(observed),
        }
    }

    fn forget_closed(&mut self, pty: PtyId) {
        if let Some(drain) = self.drains.remove(&pty) {
            drain.finish_closed();
        }
        if let Some(observed) = &self.observed {
            observed.lock().expect("native PTY observer").remove(&pty);
        }
    }

    fn abandon_open(&mut self, pty: PtyId, cause: io::Error) -> duded::ports::PtyOpenFailure {
        match self.host.close(pty) {
            Ok(_) => duded::ports::PtyOpenFailure::Failed(cause),
            Err(cleanup) => duded::ports::PtyOpenFailure::CleanupUnconfirmed {
                pty,
                cause,
                cleanup,
            },
        }
    }

    /// The child `pty` runs, as the process relation registers it.
    fn child(&self, pty: PtyId) -> io::Result<duded::ports::PtyChild> {
        let pid = self
            .host
            .pid(pty)
            .ok_or_else(|| io::Error::other("the opened PTY has no child"))?;
        Ok(duded::ports::PtyChild {
            pid,
            #[cfg(windows)]
            job: self.host.job(pty).ok(),
        })
    }
}

impl duded::ports::PtyHostPort for MainPtyHost {
    fn open(
        &mut self,
        spec: LaunchSpec,
        cols: u16,
        rows: u16,
    ) -> Result<duded::ports::OpenedPty, duded::ports::PtyOpenFailure> {
        let opened = match self.host.open(spec, dudes_pty::PtySize { cols, rows }) {
            Ok(opened) => opened,
            Err(dudes_pty::OpenError::Failed(error)) => {
                return Err(duded::ports::PtyOpenFailure::Failed(error));
            }
            Err(dudes_pty::OpenError::CleanupUnconfirmed {
                pty,
                cause,
                cleanup,
            }) => {
                return Err(duded::ports::PtyOpenFailure::CleanupUnconfirmed {
                    pty,
                    cause,
                    cleanup,
                });
            }
        };
        let pty = opened.id;
        let child = match self.child(pty) {
            Ok(child) => child,
            Err(cause) => return Err(self.abandon_open(pty, cause)),
        };
        match Drain::start(pty, opened.reader) {
            Ok(drain) => {
                self.drains.insert(pty, drain);
                if let Some(observed) = &self.observed {
                    observed.lock().expect("native PTY observer").insert(pty);
                }
                Ok(duded::ports::OpenedPty { pty, child })
            }
            Err(cause) => Err(self.abandon_open(pty, cause)),
        }
    }

    /// One call through to the host, and deliberately nothing else: no retry, no buffering and no
    /// reader bookkeeping. The count the operating system reports is the fact the daemon journals.
    fn write(&mut self, pty: PtyId, bytes: &[u8]) -> io::Result<usize> {
        self.host.write(pty, bytes)
    }

    fn resize(&mut self, pty: PtyId, cols: u16, rows: u16) -> io::Result<()> {
        self.host.resize(pty, dudes_pty::PtySize { cols, rows })
    }

    fn subscribe(&mut self, pty: PtyId) -> Option<duded::pty_output::OutputSubscription> {
        self.drains.get(&pty)?.output.subscribe()
    }

    fn close(&mut self, pty: PtyId) -> duded::ports::PtyCloseOutcome {
        match self.host.close(pty) {
            Ok(outcome) => {
                self.forget_closed(pty);
                match outcome {
                    dudes_pty::CloseOutcome::Closed => duded::ports::PtyCloseOutcome::Closed,
                    dudes_pty::CloseOutcome::AlreadyClosed => {
                        duded::ports::PtyCloseOutcome::AlreadyClosed
                    }
                }
            }
            Err(error) => duded::ports::PtyCloseOutcome::Failed(error),
        }
    }

    fn track_native_close(
        &mut self,
        pty: PtyId,
    ) -> Result<dudes_supervisor::host::NativeBoundaryReceipt, duded::ports::PtyNativeBoundaryUnknown>
    {
        use duded::ports::PtyNativeBoundaryUnknown as Unknown;
        if self.native_closes.len() == dudes_pty::MAX_CLOSE_RECEIPTS
            && !self.native_closes.contains_key(&pty)
        {
            return Err(Unknown::Limit);
        }
        let identity = self.host.track_close(pty).map_err(|_| Unknown::Identity)?;
        let entry = self
            .native_closes
            .entry(pty)
            .or_insert_with(|| NativePtyClose {
                token: dudes_supervisor::host::NativeBoundaryReceipt::fresh(),
                identity,
                receipt: None,
            });
        if entry.identity != identity {
            return Err(Unknown::Identity);
        }
        Ok(entry.token.clone())
    }

    fn native_close_absence(
        &mut self,
        pty: PtyId,
        identity: &dudes_supervisor::host::NativeBoundaryReceipt,
    ) -> Result<(), duded::ports::PtyNativeBoundaryUnknown> {
        use duded::ports::PtyNativeBoundaryUnknown as Unknown;
        let entry = self.native_closes.get_mut(&pty).ok_or(Unknown::Identity)?;
        if &entry.token != identity {
            return Err(Unknown::Identity);
        }
        entry.receipt = Some(
            self.host
                .close_receipt(entry.identity)
                .map_err(|_| Unknown::Unconfirmed)?,
        );
        Ok(())
    }

    fn acknowledge_native_close(
        &mut self,
        pty: PtyId,
        identity: &dudes_supervisor::host::NativeBoundaryReceipt,
    ) -> bool {
        let Some(entry) = self.native_closes.get(&pty) else {
            return false;
        };
        if &entry.token != identity {
            return false;
        }
        let Some(receipt) = entry.receipt else {
            return false;
        };
        if !self.host.acknowledge_close_receipt(receipt) {
            return false;
        }
        self.native_closes.remove(&pty);
        true
    }

    fn shutdown(&mut self) -> io::Result<()> {
        let result = self.host.shutdown();
        if result.is_ok() {
            for drain in std::mem::take(&mut self.drains).into_values() {
                drain.finish_closed();
            }
            if let Some(observed) = &self.observed {
                observed.lock().expect("native PTY observer").clear();
            }
        }
        result
    }
}

/// The concrete host relation, composed only beside [`RealHost`].
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
struct HostPeerProcesses(dudes_host::PeerProcesses);

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
const fn relation_of(relation: dudes_host::PeerRelation) -> duded::classify::Relation {
    match relation {
        dudes_host::PeerRelation::Matched(index) => duded::classify::Relation::Matched(index),
        dudes_host::PeerRelation::Pending(slot) => duded::classify::Relation::Pending(slot),
        dudes_host::PeerRelation::NoMatch => duded::classify::Relation::NoMatch,
        dudes_host::PeerRelation::Unresolvable => duded::classify::Relation::Unresolvable,
        dudes_host::PeerRelation::Abandoned => duded::classify::Relation::Abandoned,
    }
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
impl duded::classify::ProcessRelation for HostPeerProcesses {
    fn begin_spawn(&self) -> u64 {
        self.0.begin_spawn()
    }

    fn finish_spawn(&self, slot: u64, key: Option<&AdoptionKey>) -> bool {
        self.0.finish_spawn(slot, key)
    }

    fn published(&self, children: &[AdoptionKey]) -> Vec<(u64, usize)> {
        self.0.published(children)
    }

    fn resolve(
        &self,
        peer: u32,
        pidversion: Option<u32>,
        children: &[AdoptionKey],
    ) -> duded::classify::Relation {
        #[cfg(target_os = "macos")]
        let relation = match pidversion {
            Some(pidversion) => self.0.resolve_execution(peer, pidversion, children),
            None => dudes_host::PeerRelation::Unresolvable,
        };
        #[cfg(any(windows, target_os = "linux"))]
        let relation = {
            let _ = pidversion;
            self.0.resolve(peer, children)
        };
        relation_of(relation)
    }

    fn track_external(&self, slot: u64) -> Box<dyn duded::classify::ExternalChild> {
        Box::new(HostExternalChild(self.0.track_external(slot)))
    }
}

/// The host relation's hold on one attached PTY child, behind the daemon's port.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
struct HostExternalChild(dudes_host::ExternalChild);

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl duded::classify::ExternalChild for HostExternalChild {
    fn register(&mut self, child: duded::ports::PtyChild) -> io::Result<AdoptionKey> {
        self.0.register(child.pid)
    }
}

#[cfg(windows)]
impl duded::classify::ExternalChild for HostExternalChild {
    fn register(&mut self, child: duded::ports::PtyChild) -> io::Result<AdoptionKey> {
        let job = child.job.ok_or_else(|| {
            io::Error::other("the PTY's job could not be duplicated for the relation")
        })?;
        self.0.register(child.pid, job)
    }
}

/// The daemon's wall clock: the single source of time.
///
/// `&self`, so one value behind an `Arc` serves both the machine and the runtime; the runtime needs
/// the same reading to pick the UTC day a spend is bucketed into.
#[derive(Debug, Default)]
struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(0));
        Timestamp::from_unix_millis(millis)
    }
}

/// Ids and secrets, from the operating system's entropy and from nowhere else.
///
/// `getrandom` is named as a direct dependency rather than reached through `ulid`'s own `rand`
/// feature, and that is deliberate: the feature arrives by unification from whichever other crate
/// happened to enable it, and an agent token's unguessability must not rest on that. A panic here
/// is the right failure — a daemon that cannot read entropy must not mint a guessable credential
/// (the same defect exists from the other end, in the fakes).
#[derive(Debug, Default)]
struct OsRng;

/// 32 bytes of operating-system entropy.
fn entropy<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the operating system's entropy source");
    bytes
}

impl Rng for OsRng {
    fn ulid(&mut self) -> ulid::Ulid {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(0));
        // `from_parts` takes the low 80 bits of the random half, which is the ULID specification's
        // own width; the other 48 bits of the value are the timestamp and are not secret.
        ulid::Ulid::from_parts(millis, u128::from_le_bytes(entropy::<16>()))
    }

    fn agent_token(&mut self) -> dudes_proto::rpc::hook::HookToken {
        // 32 bytes as unpadded base64url is 43 characters, which is exactly `HOOK_TOKEN_LEN`.
        // Rendered here rather than borrowed from a base64 crate: the alphabet is eleven lines and
        // a dependency is forever.
        dudes_proto::rpc::hook::HookToken::new(base64url(&entropy::<32>()))
            .expect("43 base64url characters is a well-formed token")
    }
}

/// Unpadded base64url of exactly 32 bytes: 43 characters.
fn base64url(bytes: &[u8; 32]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(43);
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let packed =
            (u32::from(buffer[0]) << 16) | (u32::from(buffer[1]) << 8) | u32::from(buffer[2]);
        // 3 bytes make 4 characters, 2 make 3, 1 makes 2 — the unpadded lengths.
        let characters = chunk.len() + 1;
        for index in 0..characters {
            let shift = 18 - 6 * index;
            out.push(char::from(ALPHABET[((packed >> shift) & 0x3f) as usize]));
        }
    }
    out
}

/// `dudes_host::OneShot` as the daemon's [`PredicateRunner`].
///
/// The `impl` is here rather than in the library for the reason at the top of this file: this is the
/// one file that may name `dudes_host`. The trait is the daemon's and the type is the host crate's,
/// so the orphan rule allows it exactly here.
struct HostOneShot(OneShot);

impl PredicateRunner for HostOneShot {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<PredicateOutcome> {
        self.0.run(spec, timeout).map(predicate_outcome)
    }

    fn run_cancellable(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> io::Result<PredicateOutcome> {
        self.0
            .run_cancellable(spec, timeout, cancel)
            .map(predicate_outcome)
    }

    fn run_discarding_output(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
    ) -> io::Result<PredicateOutcome> {
        self.0
            .run_discarding_output(spec, timeout)
            .map(predicate_outcome)
    }

    fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        stdout_cap: usize,
    ) -> io::Result<PredicateCaptured> {
        self.0
            .run_captured(spec, timeout, stdout_cap)
            .map(|outcome| match outcome {
                CapturedOutcome::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                } => PredicateCaptured::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                },
                CapturedOutcome::Swept => PredicateCaptured::Swept,
            })
    }

    fn sweep(&self) {
        self.0.sweep();
    }
}

fn predicate_outcome(outcome: OneShotOutcome) -> PredicateOutcome {
    match outcome {
        OneShotOutcome::Finished {
            exit,
            output_tail,
            timed_out,
        } => PredicateOutcome::Finished {
            exit,
            output_tail,
            timed_out,
        },
        OneShotOutcome::Swept => PredicateOutcome::Swept,
    }
}

/// `duded::ChildEnds` as the host crate's stream sink.
///
/// The same shape and the same reason as [`HostOneShot`]: the trait is the host crate's, the type
/// is the daemon's, and this is the one file that may name `dudes_host`. What crosses here is one
/// direction of a seam that runs both ways — the daemon hands the host somewhere to send a child's
/// standard output, and the host hands back lines and an exit — so the crossing is two `impl`s and
/// no logic at all.
struct HostStreams(Arc<duded::ChildEnds>);

impl dudes_host::ChildStreams for HostStreams {
    fn watch(&self, key: Option<&AdoptionKey>) -> Box<dyn dudes_host::ChildStream> {
        Box::new(HostChildTurn(self.0.watch(key)))
    }

    fn watch_protocol(
        &self,
        key: &AdoptionKey,
        input: dudes_host::ProtocolWriter,
    ) -> Option<Box<dyn dudes_host::ChildStream>> {
        Some(Box::new(HostChildTurn(
            self.0.watch_protocol(key, Box::new(HostInput(input))),
        )))
    }

    fn protocol_failed(&self, key: &AdoptionKey) {
        self.0.protocol_failed(key);
    }
}

#[derive(Debug)]
struct HostInput(dudes_host::ProtocolWriter);

impl duded::ChildInput for HostInput {
    fn send(&self, action: dudes_providers::protocol::ProtocolAction) -> std::io::Result<()> {
        self.0.send(action)
    }
}

struct HostChildTurn(duded::ChildTurn);

impl dudes_host::ChildStream for HostChildTurn {
    fn line(&mut self, text: &str) {
        self.0.line(text);
    }

    /// The host's two fields become the journal's [`ExitStatus`], and nothing else is decided here:
    /// what an exit *means* for a turn is the provider adapter's and the supervisor's.
    fn ended(self: Box<Self>, exit: dudes_host::ChildExit) {
        self.0.ended_with_elapsed(
            ExitStatus {
                code: exit.code,
                signal: exit.signal,
            },
            exit.elapsed_ms,
        );
    }
}

fn main() -> ExitCode {
    match invocation() {
        Ok(cause) => {
            tracing::info!(?cause, "duded exited");
            ExitCode::SUCCESS
        }
        Err(error) => {
            // Standard error, because a daemon that cannot start has no journal to say so in and
            // no operator connected to be told. Everything after startup goes to `tracing`.
            let _ = write_line(&mut io::stderr(), &format!("duded: {error:#}"));
            ExitCode::FAILURE
        }
    }
}

/// Write `text` escaped, newline included, in one `write_all`.
///
/// One call because quickstart appends its spawn marker to the log file that is this daemon's
/// standard error. `eprintln!` makes one write per formatting piece on the unbuffered stream, so
/// the marker could land inside a line written that way.
fn write_line(sink: &mut impl io::Write, text: &str) -> io::Result<()> {
    sink.write_all(format!("{}\n", Escaped(text)).as_bytes())
}

fn invocation() -> anyhow::Result<duded::ExitCause> {
    use std::io::Write as _;

    let mut args = std::env::args_os().skip(1);
    if let Some(option) = args.next() {
        if (option == "--version" || option == "-V") && args.next().is_none() {
            write_line(
                &mut io::stdout(),
                concat!("duded ", env!("CARGO_PKG_VERSION")),
            )?;
            return Ok(duded::ExitCause::Ended);
        }
        anyhow::ensure!(
            option == "--check-floor",
            "unknown argument {}; use `duded --check-floor <absolute-directory>` or no arguments",
            option.to_string_lossy()
        );
        let path = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("--check-floor needs an absolute directory"))?;
        anyhow::ensure!(
            args.next().is_none(),
            "--check-floor takes one absolute directory"
        );
        let assets = duded::floor_gateway::prepare(&duded::settings::FloorSettings {
            assets_dir: PathBuf::from(path),
            port: None,
        })?;
        let output = serde_json::to_vec(&assets.release_files())?;
        std::io::stdout().lock().write_all(&output)?;
        return Ok(duded::ExitCause::Ended);
    }
    start()
}

/// Compose the seams and run.
fn start() -> anyhow::Result<duded::ExitCause> {
    // **Before anything else spawns a thread.** A signal mask is per-thread and is inherited at
    // thread creation, so blocking `SIGTERM` here — ahead of the accept loop, the reaper and every
    // predicate — is what makes it deliverable to the waiter below rather than to whichever thread
    // the kernel picked, where its default disposition would end the process before `shutdown` ran.
    // Measured: with the block moved *after* `boot`, a `SIGTERM`ed daemon dies of signal 15.
    #[cfg(unix)]
    let blocked = block_stop_signals()?;

    let (settings, providers, memory, options) = settings_from_environment()?;
    // The sink first: the host is handed it at construction, and `boot` gives it the way back into
    // the decide loop as soon as the loop's queue exists.
    let children = duded::ChildEnds::new();
    let host = RealHost::watching(Arc::new(HostStreams(Arc::clone(&children))));
    // Supported desktop platforms use the host-owned process-ancestry relation. Other targets keep the
    // explicit `NoWalk -> Unresolvable` fallback and publish no child mapping.
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    let resolver =
        duded::classify::Resolver::processes(Arc::new(HostPeerProcesses(host.peer_processes())));
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let resolver = duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk));
    // Between the executor and the host, so the barrier is armed before the exec and released
    // after it. Inside the fault wrappers below on purpose: a stalled launch that never reaches
    // the real host must still release its slot.
    let host = duded::classify::PeerBarrier::new(host, resolver.clone());
    // The end-to-end gate's crash arms ask the loop to stop in one of the two windows around a launch: the
    // one between a turn's fsynced append and its launch, and the one between the launch and the
    // record naming it. The wrappers are absent from every build that did not ask for the feature,
    // and their configuration is read here because this is the file that reads the ambient
    // environment.
    //
    // Composed rather than merged, and the order is the meaning: the before-fault is the inner
    // wrapper, so if an operator arms both on the same launch the loop stops in the earlier window
    // — the one with no child in it — rather than in whichever the composition happened to reach
    // first. Each counts the launches it sees, and neither arm arms both.
    #[cfg(feature = "fault-injection")]
    let host = stall_after_spawn_from_environment(stall_before_spawn_from_environment(host));
    let predicates = Arc::new(HostOneShot(OneShot::new()));
    let seams = Seams {
        host,
        pty_host: Box::new(MainPtyHost::new()),
        predicates: Arc::clone(&predicates),
        clock: Arc::new(SystemClock),
        rng: OsRng,
        resolver,
        // Built here because this is the file that reads the ambient environment, and an
        // `isolation = "bare"` table is honourable only against one that carries an API-key path.
        // A table the adapter cannot honour therefore ends startup with the reason on stderr,
        // rather than costing a turn to discover.
        adapters: providers_table(&settings.daemon_env, providers, &settings.state_dir).map_err(
            |error| anyhow::anyhow!("the [providers] configuration was refused: {error}"),
        )?,
        children,
        witness: Witness::default(),
    };
    let daemon =
        duded::boot::boot_with_memory_config_and_options(&settings, seams, memory, options)?;
    announce(
        daemon.socket(),
        settings.operator_config.as_deref(),
        daemon.floor_addr(),
    );
    for (agent, row) in &daemon.world().agents {
        if !row.retired && row.cwd.is_relative() {
            let _ = write_line(
                &mut io::stderr(),
                &format!(
                    "duded: agent {agent} has a legacy relative working directory; inspect it and \
                     retire/rehire with an absolute --cwd if appropriate"
                ),
            );
        }
    }

    #[cfg(unix)]
    watch_stop_signals(blocked, daemon.handle())?;
    daemon.run()
}

/// One line saying what this daemon serves, once it serves it: the version, the control socket,
/// the operator file and the Floor. Every value is non-secret; the bearer only `floor open` mints
/// is not among them. A quickstarted daemon's standard error is its log.
fn announce(
    socket: &std::path::Path,
    config: Option<&std::path::Path>,
    floor: Option<std::net::SocketAddr>,
) {
    let config = config.map_or_else(
        || "config unavailable".to_owned(),
        |path| {
            let absent = if path.exists() { "" } else { " (absent)" };
            format!("config {}{absent}", path.display())
        },
    );
    let floor = floor.map_or_else(
        || {
            "Floor off: install the built Floor as `floor` beside duded (`cargo xtask dist` lays \
             one out) or as `share/dudes/floor` beside duded's `bin` directory (install.sh puts \
             one there), or start duded directly with DUDES_FLOOR_ASSETS"
                .to_owned()
        },
        |addr| {
            format!("Floor at http://{addr}/floor/, sign in with a link from `dudes floor open`")
        },
    );
    // A daemon that cannot write its own log line keeps serving rather than stop over it.
    let _ = write_line(
        &mut io::stderr(),
        &format!(
            "duded {} on {}; {config}; {floor}",
            env!("CARGO_PKG_VERSION"),
            socket.display()
        ),
    );
}

/// Block `SIGTERM` and `SIGINT` in this thread, and therefore in every thread started after it.
///
/// Nothing here installs a handler, so nothing here needs `unsafe` — the workspace forbids it in
/// this package, and `nix` does the two calls (`pthread_sigmask` and, below, `sigwait`) behind a
/// safe surface. That is the same reason `dudes-rpc` reaches peer credentials through `nix`.
#[cfg(unix)]
fn block_stop_signals() -> anyhow::Result<nix::sys::signal::SigSet> {
    use nix::sys::signal::{SigSet, Signal};

    let mut set = SigSet::empty();
    set.add(Signal::SIGTERM);
    set.add(Signal::SIGINT);
    set.thread_block()?;
    Ok(set)
}

/// Wait for one of the blocked signals on a thread of the daemon's own, and ask it to stop.
#[cfg(unix)]
fn watch_stop_signals(
    blocked: nix::sys::signal::SigSet,
    handle: duded::Handle,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    Ok(std::thread::Builder::new()
        .name("dudes-signals".to_owned())
        .spawn(move || match blocked.wait() {
            Ok(signal) => {
                tracing::info!(?signal, "a stop signal arrived");
                handle.stop(duded::ExitCause::Signal);
            }
            Err(errno) => tracing::error!(%errno, "the signal wait failed"),
        })?)
}

#[cfg(all(test, unix))]
mod pty_tests {
    use std::time::Instant;

    use duded::ports::{PtyCloseOutcome, PtyHostPort as _};
    use duded::pty_output::{MAX_FRAME_BYTES, Output, OutputSubscription};

    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    fn open(host: &mut MainPtyHost, cwd: &std::path::Path, argv: &[&str]) -> PtyId {
        let spec = LaunchSpec {
            argv: argv.iter().map(Into::into).collect(),
            env: vec![EnvVar::new("PATH", "/usr/bin:/bin")],
            cwd: cwd.to_path_buf(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        };
        match host.open(spec, 80, 24) {
            Ok(opened) => opened.pty,
            Err(failure) => panic!("the PTY did not open: {failure:?}"),
        }
    }

    pub(super) fn seq_output(last: u32) -> Vec<u8> {
        let mut lines = Vec::new();
        for n in 1..=last {
            lines.extend_from_slice(n.to_string().as_bytes());
            lines.push(b'\n');
        }
        lines
    }

    /// The terminal's own output processing is dropped: macOS sometimes emits one newline's
    /// carriage return twice when the terminal's output queue fills mid-conversion.
    pub(super) fn without_cr(screen: &[u8]) -> Vec<u8> {
        screen
            .iter()
            .copied()
            .filter(|byte| *byte != b'\r')
            .collect()
    }

    pub(super) fn read_to_end(output: &OutputSubscription) -> (Vec<u8>, Output) {
        let mut screen = Vec::new();
        loop {
            match output.next(WAIT) {
                Output::Data(bytes) => {
                    assert!(bytes.len() <= MAX_FRAME_BYTES);
                    screen.extend(bytes);
                }
                other => return (screen, other),
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn read_until(output: &OutputSubscription, screen: &mut Vec<u8>, needle: &[u8]) {
        let deadline = Instant::now() + WAIT;
        while !screen.windows(needle.len()).any(|window| window == needle) {
            match output.next(deadline.saturating_duration_since(Instant::now())) {
                Output::Data(bytes) => screen.extend(bytes),
                other => panic!(
                    "{other:?} before {:?}; screen: {:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(screen)
                ),
            }
        }
    }

    #[test]
    fn a_real_childs_output_reaches_its_subscriber_in_order() {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut host = MainPtyHost::new();
        let pty = open(&mut host, temp.path(), &["/usr/bin/seq", "1", "50000"]);
        let output = host.subscribe(pty).expect("the first subscriber");
        let (screen, last) = read_to_end(&output);
        assert_eq!(last, Output::Ended);
        let (screen, expected) = (without_cr(&screen), seq_output(50_000));
        let first_difference = screen
            .iter()
            .zip(&expected)
            .position(|(seen, want)| seen != want);
        assert_eq!(first_difference, None);
        assert_eq!(screen.len(), expected.len());
        assert!(matches!(host.close(pty), PtyCloseOutcome::Closed));
        assert!(host.subscribe(pty).is_none(), "a closed PTY offered output");
    }

    #[test]
    fn output_nobody_reads_closes_its_channel_at_the_bound() {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut host = MainPtyHost::new();
        let pty = open(&mut host, temp.path(), &["/usr/bin/yes"]);
        let output = host.subscribe(pty).expect("subscriber");
        let deadline = Instant::now() + WAIT;
        loop {
            std::thread::sleep(Duration::from_millis(100));
            match output.next(Duration::ZERO) {
                Output::Overflowed => break,
                Output::Data(_) | Output::Idle => {
                    assert!(
                        Instant::now() < deadline,
                        "the bound never closed the channel"
                    );
                }
                Output::Ended => panic!("an endless child ended"),
            }
        }
        assert_eq!(output.next(Duration::ZERO), Output::Overflowed);
        assert!(matches!(host.close(pty), PtyCloseOutcome::Closed));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod native_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    use dudes_journal::{Fsync, Reader};
    use dudes_proto::rpc::Method;
    use dudes_proto::rpc::agent::{AttachParams, AttachResult, HireParams, HireResult};
    use dudes_proto::rpc::cred::OperatorCredential;
    use dudes_proto::rpc::goal::GoalSetParams;
    use dudes_proto::{Actor, AgentBudget, GoalBudget, PeerProvenance, Record, Spend};
    use dudes_providers::registry::ProvidersConfig;
    use dudes_rpc::Client;
    use dudes_types::{Lsn, Provider, Tokens, Usd};

    /// Each agent's session is this followed by its one-letter marker, a hexadecimal digit.
    const SESSION_PREFIX: &str = "3f9a1c2e-0000-4000-8000-00000000396";
    const HELPER_ROLE: &str = "DUDED_NATIVE_HELPER_OPERATOR";
    const HELPER_STATE: &str = "DUDED_NATIVE_HELPER_STATE";
    const HELPER_RUNTIME: &str = "DUDED_NATIVE_HELPER_RUNTIME";
    const HELPER_RESULT: &str = "DUDED_NATIVE_HELPER_RESULT";
    const HELPER_RELEASE: &str = "DUDED_NATIVE_HELPER_RELEASE";
    const ATTACHED_STAND_IN: &str = "if [ -t 0 ]; then\n  trap 'echo \"size $(stty size)\"' WINCH\n  echo \"agent $(cat marker)\"\n  /usr/bin/seq 1 3000\n  echo \"size $(stty size)\"\n  while :; do sleep 1; done\nfi\n";

    fn records(dir: &std::path::Path) -> Vec<Record> {
        Reader::new(dir)
            .iter_from(Lsn::ZERO)
            .expect("read native-test journal")
            .map(|item| item.expect("journal entry").1.record)
            .collect()
    }

    fn await_record<T>(dir: &std::path::Path, select: impl Fn(&Record) -> Option<T>) -> T {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(found) = records(dir).iter().find_map(&select) {
                return found;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "record did not arrive"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Hire one agent whose workspace holds `marker`, and give it a confirmed session.
    fn hire_with_session(
        client: &mut dudes_rpc::LocalClient,
        credential: &str,
        state: &std::path::Path,
        marker: &str,
    ) -> dudes_types::AgentId {
        let cwd = state.join(marker);
        std::fs::create_dir_all(&cwd).expect("agent workspace");
        std::fs::write(cwd.join("marker"), marker).expect("agent marker");
        let call = |client: &mut dudes_rpc::LocalClient, method, params: serde_json::Value| {
            let response = client
                .call_as_operator(
                    method,
                    Some(params),
                    OperatorCredential::new(credential.to_owned()).expect("credential shape"),
                )
                .expect("helper response");
            response
                .result
                .unwrap_or_else(|| panic!("{method:?} failed: {:?}", response.error))
        };
        let hire: HireResult = serde_json::from_value(call(
            client,
            Method::AgentHire,
            serde_json::to_value(HireParams {
                character: None,
                accent: None,
                name: format!("Native {marker}"),
                role: "tester".into(),
                department: "engineering".into(),
                provider: Provider::Claude,
                cwd,
                budget: AgentBudget {
                    usd_per_day: Some(Usd::from_dollars(1)),
                    tokens_per_day: None,
                },
                sandbox: None,
                isolation: None,
                max_turns: None,
                max_budget: None,
                trusted: false,
                checkout: None,
                from: None,
                mcp_servers: Vec::new(),
            })
            .unwrap(),
        ))
        .unwrap();
        call(
            client,
            Method::GoalSet,
            serde_json::to_value(GoalSetParams {
                agent: hire.agent,
                text: "establish a confirmed session".into(),
                predicate: dudes_proto::goal::Predicate::Command {
                    argv: vec!["/usr/bin/true".into()],
                    timeout_s: 10,
                },
                budget: GoalBudget {
                    usd: Some(Usd::from_dollars(1)),
                    tokens: None,
                },
                hop_cap: 1,
                check: None,
                may_already_pass: true,
            })
            .unwrap(),
        );
        await_record(
            &state.join(duded::boot::JOURNAL_DIR),
            |record| match record {
                Record::TurnEnded { agent, .. } if *agent == hire.agent => Some(()),
                _ => None,
            },
        );
        hire.agent
    }

    #[test]
    fn native_operator_helper() {
        let Some(role) = std::env::var_os(HELPER_ROLE) else {
            return;
        };
        assert_eq!(role, "operator");
        let state = PathBuf::from(std::env::var_os(HELPER_STATE).expect("helper state"));
        let runtime = PathBuf::from(std::env::var_os(HELPER_RUNTIME).expect("helper runtime"));
        let result_path = PathBuf::from(std::env::var_os(HELPER_RESULT).expect("helper result"));
        let release = PathBuf::from(std::env::var_os(HELPER_RELEASE).expect("helper release"));
        let socket = dudes_rpc::local_endpoint(&runtime, SOCKET_NAME);
        let credential = std::fs::read_to_string(runtime.join(dudes_rpc::OPERATOR_CREDENTIAL_NAME))
            .expect("helper credential");
        // Each connection owns at most one attachment, and only a connection accepted before the
        // first hire holds operator effect authority, so both connect and are answered first.
        let mut owners = [
            Client::connect(&socket).expect("first outside operator connects before hire"),
            Client::connect(&socket).expect("second outside operator connects before hire"),
        ];
        for owner in &mut owners {
            let ping = owner.call(Method::SystemPing, None).expect("ping response");
            assert!(
                ping.error.is_none(),
                "a pre-hire connection was refused: {ping:?}"
            );
        }
        let agents = [
            hire_with_session(&mut owners[0], &credential, &state, "a"),
            hire_with_session(&mut owners[1], &credential, &state, "b"),
        ];
        let mut published = Vec::new();
        for (owner, agent) in owners.iter_mut().zip(agents) {
            let response = owner
                .call_as_operator(
                    Method::AgentAttach,
                    Some(
                        serde_json::to_value(AttachParams {
                            agent,
                            cols: 80,
                            rows: 24,
                        })
                        .unwrap(),
                    ),
                    OperatorCredential::new(credential.clone()).expect("credential shape"),
                )
                .expect("helper attach response");
            let Some(result) = response.result else {
                std::fs::write(&result_path, format!("helper attach failed: {response:?}"))
                    .expect("publish helper failure");
                return;
            };
            let attached: AttachResult = serde_json::from_value(result).unwrap();
            published.push(format!("{agent} {}", attached.pty.0));
        }
        std::fs::write(&result_path, published.join(" ")).expect("publish attachments");
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !release.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "helper release arrived"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Two attached agents' PTYs at once, through the production daemon, process-ancestry resolver and main
    /// PTY adapter: each subscriber sees exactly its own child's output, and a resize reaches only
    /// the PTY it names.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the native control keeps production composition and its observations together"
    )]
    fn native_agent_attach_crosses_production_r4_and_main_pty_adapter() {
        let state = tempfile::tempdir().expect("state dir");
        let runtime = tempfile::tempdir().expect("runtime dir");
        let stand_in = state.path().join("claude-stand-in");
        let script = format!(
            "#!/bin/sh\n{ATTACHED_STAND_IN}S={SESSION_PREFIX}$(cat marker)\nprintf '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"%s\",\"tools\":[]}}\\n' \"$S\"\nprintf '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"%s\",\"result\":\"done\",\"total_cost_usd\":0.0,\"modelUsage\":{{}}}}\\n' \"$S\"\n"
        );
        std::fs::write(&stand_in, script).expect("stand-in source");
        std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o700))
            .expect("stand-in executable");

        let settings = Settings {
            state_dir: state.path().to_path_buf(),
            runtime_dir: runtime.path().join("run"),
            socket_name: SOCKET_NAME.to_owned(),
            journal: JournalOptions {
                fsync: Fsync::Full,
                ..JournalOptions::default()
            },
            floor_usd_per_day: Some(Usd::from_dollars(100)),
            per_turn_floor: Spend {
                usd: Usd::from_micros(1),
                tokens: Tokens::new(1),
                work_tokens: Tokens::new(1),
            },
            enrollment: duded::floor_actions::Enrollment::default(),
            operator_config: None,
            floor_answer_enabled: false,
            floor_hire_enabled: false,
            floor_control: duded::settings::FloorControl::Off,
            floor_recovery_enabled: false,
            floor_board_enabled: false,
            floor_terminal_enabled: false,
            floor_git_enabled: false,
            floor_diff_enabled: false,
            condense_model_enabled: true,
            condense_model: None,
            breaker: BreakerConfig::default(),
            daemon_env: vec![EnvVar::new("PATH", "/usr/bin:/bin")],
            predicate_env: vec![EnvVar::new("PATH", "/usr/bin:/bin")],
            predicate_path: Some("/usr/bin:/bin".into()),
            git_checkout_deadline: duded::worktree::git::CHECKOUT_DEADLINE,
            max_connections: NonZeroUsize::new(8).expect("nonzero"),
            beat: Duration::from_hours(1),
            floor: None,
            hook_command: Some({
                let path = state.path().join("dudes-hook");
                std::fs::write(&path, b"").expect("a stand-in hook command");
                path
            }),
            daemon_exe: None,
        };
        let children = duded::ChildEnds::new();
        let host = RealHost::watching(Arc::new(HostStreams(Arc::clone(&children))));
        let resolver = duded::classify::Resolver::processes(Arc::new(HostPeerProcesses(
            host.peer_processes(),
        )));
        let host = duded::classify::PeerBarrier::new(host, resolver.clone());
        let observed = Arc::new(Mutex::new(BTreeSet::new()));
        let seams = Seams {
            host,
            pty_host: Box::new(MainPtyHost::observed(Arc::clone(&observed))),
            predicates: Arc::new(HostOneShot(OneShot::new())),
            clock: Arc::new(SystemClock),
            rng: OsRng,
            resolver,
            adapters: Adapters::from_config(
                &ProvidersConfig {
                    claude: Some(ClaudeConfig {
                        command: stand_in.into_os_string(),
                        ..ClaudeConfig::default()
                    }),
                },
                &settings.daemon_env,
            )
            .expect("account-free adapter"),
            children,
            witness: Witness::default(),
        };
        let daemon = duded::boot(&settings, seams).expect("boot");
        let handle = daemon.handle();
        let thread = std::thread::spawn(move || daemon.run());

        let result_path = state.path().join("native-attached-ids");
        let release_path = state.path().join("release-native-operator");
        let mut helper = Some(std::thread::spawn({
            let state_path = state.path().to_path_buf();
            let runtime_path = settings.runtime_dir.clone();
            let result_path = result_path.clone();
            let release_path = release_path.clone();
            move || {
                let spec = LaunchSpec {
                    argv: vec![
                        std::env::current_exe()
                            .expect("test executable")
                            .into_os_string(),
                        "--exact".into(),
                        "native_tests::native_operator_helper".into(),
                        "--quiet".into(),
                    ],
                    env: vec![
                        EnvVar::new("PATH", "/usr/bin:/bin"),
                        EnvVar::new(HELPER_ROLE, "operator"),
                        EnvVar::new(HELPER_STATE, state_path.as_os_str()),
                        EnvVar::new(HELPER_RUNTIME, runtime_path.as_os_str()),
                        EnvVar::new(HELPER_RESULT, result_path.as_os_str()),
                        EnvVar::new(HELPER_RELEASE, release_path.as_os_str()),
                    ],
                    cwd: state_path,
                    stdin: dudes_providers::launch::LaunchInput::Null,
                };
                OneShot::new()
                    .run(&spec, Duration::from_secs(40))
                    .expect("outside operator helper")
            }
        }));
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let attached: Vec<(dudes_types::AgentId, PtyId)> = loop {
            if let Ok(text) = std::fs::read_to_string(&result_path) {
                let words: Vec<&str> = text.split(' ').collect();
                let pairs: Option<Vec<_>> = words
                    .chunks(2)
                    .map(|pair| {
                        Some((
                            dudes_types::AgentId::parse(pair.first()?).ok()?,
                            PtyId(pair.get(1)?.parse().ok()?),
                        ))
                    })
                    .collect();
                match pairs {
                    Some(pairs) if pairs.len() == 2 => break pairs,
                    _ if text.starts_with("helper attach failed") => panic!("{text}"),
                    _ => {}
                }
            }
            if helper.as_ref().expect("helper retained").is_finished() {
                let outcome = helper
                    .take()
                    .expect("helper retained")
                    .join()
                    .expect("outside operator helper thread");
                panic!("outside operator helper ended before attach: {outcome:?}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "native attach results arrived"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let [(agent_a, pty_a), (agent_b, pty_b)] = [attached[0], attached[1]];
        assert_ne!(pty_a, pty_b);
        let journal = state.path().join(duded::boot::JOURNAL_DIR);
        assert_eq!(
            *observed.lock().expect("native PTY observer"),
            BTreeSet::from([pty_a, pty_b]),
            "the returned ids name the main adapter's native PTYs"
        );
        assert_eq!(
            records(&journal)
                .iter()
                .filter(|record| matches!(
                    record,
                    Record::Attached {
                        by: Actor::Peer { .. },
                        provenance: Some(PeerProvenance::NoRecordedAgent),
                        ..
                    }
                ))
                .count(),
            2
        );
        assert!(
            handle.subscribe_output(agent_a, pty_b).is_err(),
            "one agent's handle does not name another agent's PTY"
        );

        let screen_of = |marker: &str, sizes: &[&str]| {
            let mut screen = format!("agent {marker}\n").into_bytes();
            screen.extend(super::pty_tests::seq_output(3000));
            screen.extend(sizes.join("\n").into_bytes());
            screen
        };
        let exact = |screen: &[u8], expected: &[u8]| {
            let text = super::pty_tests::without_cr(screen);
            assert_eq!(
                String::from_utf8_lossy(text.strip_suffix(b"\n").unwrap_or(&text)),
                String::from_utf8_lossy(expected)
            );
        };
        let output_a = handle
            .subscribe_output(agent_a, pty_a)
            .expect("agent a's PTY output through the daemon");
        let output_b = handle
            .subscribe_output(agent_b, pty_b)
            .expect("agent b's PTY output through the daemon");
        let (mut screen_a, mut screen_b) = (Vec::new(), Vec::new());
        super::pty_tests::read_until(&output_a, &mut screen_a, b"size 24 80\r");
        super::pty_tests::read_until(&output_b, &mut screen_b, b"size 24 80\r");
        exact(&screen_a, &screen_of("a", &["size 24 80"]));
        exact(&screen_b, &screen_of("b", &["size 24 80"]));

        let resize = |agent, pty, cols, rows| {
            handle.ask(duded::ports::Ask::PtyResize {
                agent,
                pty,
                cols,
                rows,
            })
        };
        assert!(resize(agent_a, pty_b, 132, 50).is_err(), "a crossed resize");
        assert!(matches!(
            resize(agent_a, pty_a, 132, 50),
            Ok(duded::ports::Answered::Done)
        ));
        super::pty_tests::read_until(&output_a, &mut screen_a, b"size 50 132\r");
        assert!(matches!(
            resize(agent_b, pty_b, 100, 30),
            Ok(duded::ports::Answered::Done)
        ));
        super::pty_tests::read_until(&output_b, &mut screen_b, b"size 30 100\r");
        exact(&screen_a, &screen_of("a", &["size 24 80", "size 50 132"]));
        exact(&screen_b, &screen_of("b", &["size 24 80", "size 30 100"]));

        std::fs::write(&release_path, b"release").expect("release outside operator");
        let helper = helper
            .take()
            .expect("helper retained")
            .join()
            .expect("outside operator helper thread");
        assert!(matches!(
            helper,
            OneShotOutcome::Finished {
                exit: Some(0),
                timed_out: false,
                ..
            }
        ));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while records(&journal)
            .iter()
            .filter(|record| {
                matches!(
                    record,
                    Record::Detached {
                        by: Actor::System,
                        ..
                    }
                )
            })
            .count()
            < 2
        {
            assert!(
                std::time::Instant::now() < deadline,
                "both attachments were detached"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(observed.lock().expect("native PTY observer").is_empty());
        for output in [&output_a, &output_b] {
            let (rest, last) = super::pty_tests::read_to_end(output);
            assert_eq!(last, duded::pty_output::Output::Ended);
            assert!(
                super::pty_tests::without_cr(&rest)
                    .iter()
                    .all(|byte| *byte == b'\n'),
                "no bytes arrived after the last size line: {:?}",
                String::from_utf8_lossy(&rest)
            );
        }
        handle.stop(duded::ExitCause::Ended);
        assert_eq!(
            thread.join().expect("daemon thread").expect("daemon run"),
            duded::ExitCause::Ended
        );
    }
}

/// The adapters this daemon runs on: Claude on its defaults, and Codex or Pi when the operator's
/// file has a `[providers.codex]` or `[providers.pi]` table.
///
/// A provider with no adapter here is refused at `agent.hire` instead of journaling an agent that
/// could never take a turn.
///
/// A `stub-provider` build refuses both tables at startup, for the reason [`configured_claude`]
/// refuses a missing stub: the stub speaks only Claude's protocol, so a Codex or Pi adapter in that
/// build could only run the real CLI and spend real vendor credit.
fn providers_table(
    daemon_env: &[EnvVar],
    providers: ProviderTables,
    state_dir: &std::path::Path,
) -> anyhow::Result<Adapters> {
    let claude = configured_claude(daemon_env)?;
    claude.check(daemon_env)?;
    #[cfg(feature = "stub-provider")]
    for (name, present) in [
        ("codex", providers.codex.is_some()),
        ("pi", providers.pi.is_some()),
    ] {
        anyhow::ensure!(
            !present,
            "this daemon was built with the `stub-provider` feature, which has no {name} stub; \
             remove [providers.{name}] rather than run the real {name} CLI"
        );
    }
    let ProviderTables { codex, pi } = providers;
    Ok(Adapters::new(
        std::iter::once(Box::new(ClaudeAdapter::new(claude)) as Box<dyn Adapter>)
            .chain(codex.map(|codex| {
                Box::new(
                    CodexAdapter::new(codex).with_managed_home(state_dir.join("providers/codex")),
                ) as Box<dyn Adapter>
            }))
            .chain(pi.map(|pi| Box::new(PiAdapter::new(pi)) as Box<dyn Adapter>)),
    ))
}

/// The adapter configuration a released daemon runs on, and the only one it can run on.
///
/// Infallible here and fallible under the feature, so the two arms share one signature and the
/// caller has no branch on which build it is.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the `stub-provider` arm of this function can fail, and both arms share a signature"
)]
#[cfg(not(feature = "stub-provider"))]
fn configured_claude(_daemon_env: &[EnvVar]) -> anyhow::Result<ClaudeConfig> {
    Ok(ClaudeConfig::default())
}

/// The `stub-provider` build's configuration: the stub program, or no daemon at all.
///
/// **There is no fall-through, and refusing it is the whole point of this function.**
/// [`ClaudeConfig::default`] names `claude`, so a feature-on daemon that could not find its stub
/// command would go on launching the real CLI and spending real vendor credit — which is the cost
/// this feature exists to remove. A build made to run against a stub and unable to find one does
/// not start, and says so on stderr rather than discovering it a turn later.
///
/// The name is read from the daemon's own environment, handed in rather than looked up, and it is
/// not in `dudes`' forwarding allowlist (`bins/dudes/src/env.rs`), so an interactive session
/// cannot arm it either — the rule the crash faults already follow.
#[cfg(feature = "stub-provider")]
fn configured_claude(daemon_env: &[EnvVar]) -> anyhow::Result<ClaudeConfig> {
    let command = daemon_env
        .iter()
        .find(|pair| pair.name() == std::ffi::OsStr::new(STUB_COMMAND_VAR))
        .map(|pair| pair.expose_value().to_os_string())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "this daemon was built with the `stub-provider` feature and {STUB_COMMAND_VAR} is \
                 unset; it will not fall back to the real provider CLI"
            )
        })?;
    anyhow::ensure!(
        !command.is_empty(),
        "{STUB_COMMAND_VAR} is empty; refusing rather than falling back to the real provider CLI"
    );
    Ok(ClaudeConfig {
        command,
        ..ClaudeConfig::default()
    })
}

/// Where the `stub-provider` build reads its stub program from.
#[cfg(feature = "stub-provider")]
const STUB_COMMAND_VAR: &str = "DUDES_STUB_PROVIDER_COMMAND";

/// Which launch a fault is armed on and where it announces itself, read from the environment.
///
/// `variable` is the fault's own launch-number variable, counting from 1;
/// `DUDES_FAULT_STALL_MARKER` is the file to create when the loop reaches the window, and is shared
/// by both faults because no arm arms two at once and a second marker name would be a second thing
/// to keep in step. Neither set, or the number unparseable, means no fault — a variable with a typo
/// must not silently arm a stall, and must not silently *disarm* one either, which is why the
/// refusal is logged rather than swallowed. `0` is "never": the counter starts at 1, so no launch
/// can equal it.
#[cfg(feature = "fault-injection")]
fn armed_launch(variable: &str) -> (u32, PathBuf) {
    const MARKER: &str = "DUDES_FAULT_STALL_MARKER";

    let nth = match std::env::var(variable) {
        Ok(value) => value.parse::<u32>().unwrap_or_else(|error| {
            tracing::error!(%error, value, "{variable} is not a launch number; no fault is armed");
            0
        }),
        Err(_) => 0,
    };
    match (nth, std::env::var_os(MARKER).map(PathBuf::from)) {
        (0, _) | (_, None) => (0, PathBuf::new()),
        (nth, Some(marker)) => {
            tracing::warn!(nth, marker = %marker.display(), "fault-injection is armed: {variable}");
            (nth, marker)
        }
    }
}

/// Wrap the host so the loop stalls before one launch, when the environment asks for it.
///
/// `DUDES_FAULT_STALL_BEFORE_SPAWN` is which launch. Unarmed, the host is handed back untouched.
///
/// Only compiled with the `fault-injection` feature. See [`duded::fault`] for why the gate needs
/// this and why the daemon does not kill itself.
#[cfg(feature = "fault-injection")]
fn stall_before_spawn_from_environment<H: dudes_supervisor::host::ProcessHost>(
    host: H,
) -> duded::fault::StallBeforeSpawn<H> {
    let (nth, marker) = armed_launch("DUDES_FAULT_STALL_BEFORE_SPAWN");
    duded::fault::StallBeforeSpawn::new(host, nth, marker)
}

/// Wrap the host so the loop stalls *after* one launch, with that launch's child alive.
///
/// `DUDES_FAULT_STALL_AFTER_SPAWN` is which launch. Unarmed, the host is handed back untouched.
///
/// Only compiled with the `fault-injection` feature. The window this one opens — a real child that
/// no record names — is the one a post-spawn crash test needs, and [`duded::fault`] says why it
/// cannot be reached from outside.
#[cfg(feature = "fault-injection")]
fn stall_after_spawn_from_environment<H: dudes_supervisor::host::ProcessHost>(
    host: H,
) -> duded::fault::StallAfterSpawn<H> {
    let (nth, marker) = armed_launch("DUDES_FAULT_STALL_AFTER_SPAWN");
    duded::fault::StallAfterSpawn::new(host, nth, marker)
}

/// Everything ambient, read once.
///
/// The two data directories keep their environment overrides. The operator configuration follows
/// the platform path documented for `config.toml` and is loaded before persistent or child work.
fn settings_from_environment() -> anyhow::Result<(
    Settings,
    ProviderTables,
    dudes_journal::MemoryCondenseConfig,
    duded::settings::RunOptions,
)> {
    #[cfg(feature = "fault-injection")]
    let recovery_stop = duded::fault::RecoveryStop::parse(
        std::env::var_os("DUDES_FAULT_RECOVERY_STOP").as_deref(),
    )?;
    let profile = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        None
    };
    let root = storage_root_from_values(std::env::var_os("HOME"), profile);
    let state_dir = state_dir_from_values(std::env::var_os("DUDES_STATE_DIR"), root.as_deref())?;
    let runtime_dir = directory_from_values(
        std::env::var_os("DUDES_RUNTIME_DIR"),
        root.as_deref(),
        "DUDES_RUNTIME_DIR",
        ".local/state/dudes",
    )?;
    let operator_config = operator_config_path()?;
    let (startup, memory) = load_operator_and_memory_with_options(&operator_config)?;
    let operator = startup.operator;
    let budget = operator.budget;
    // The build and an install put `dudes-hook` beside this binary and the Floor bundle where
    // `installed_floor` looks. Without a path to it, attaching is refused and no installed Floor
    // is served.
    let exe = std::env::current_exe()
        .inspect_err(|error| tracing::warn!(%error, "no path to this binary"))
        .ok();
    let floor_assets = std::env::var_os("DUDES_FLOOR_ASSETS");
    // An explicit bundle wins, so an installed one is not even inspected: an unsafe installed
    // bundle must not stop a daemon that was pointed elsewhere.
    let installed_floor = match (&floor_assets, &exe) {
        (None, Some(exe)) => duded::settings::installed_floor(exe, effective_uid())?,
        _ => None,
    };
    // The whole of this process's environment, as the adapter's scrub expects: it is what a
    // managed child *starts from*, and the adapter is what narrows it.
    let daemon_env: Vec<EnvVar> = std::env::vars_os()
        .map(|(name, value)| EnvVar::new(name, value))
        .collect();
    #[cfg(unix)]
    let daemon_env = with_user_name(daemon_env);
    let settings = Settings {
        state_dir,
        runtime_dir,
        socket_name: SOCKET_NAME.to_owned(),
        journal: JournalOptions::default(),
        floor_usd_per_day: budget.floor_usd_per_day,
        per_turn_floor: budget.per_turn_floor,
        enrollment: operator.enrollment,
        operator_config: Some(operator_config),
        floor_answer_enabled: operator.answer_enabled,
        floor_hire_enabled: operator.hire_enabled,
        floor_control: operator.control,
        floor_recovery_enabled: operator.recovery_enabled,
        floor_board_enabled: operator.board_enabled,
        floor_terminal_enabled: operator.terminal_enabled,
        floor_git_enabled: operator.git_enabled,
        floor_diff_enabled: operator.diff_enabled,
        condense_model_enabled: operator.condense_model_enabled,
        condense_model: operator.condense_model,
        breaker: BreakerConfig::default(),
        daemon_env,
        // A predicate child gets the allowlist and nothing else.
        predicate_env: allowlisted(std::env::vars_os()),
        predicate_path: std::env::var_os("PATH"),
        git_checkout_deadline: duded::worktree::git::CHECKOUT_DEADLINE,
        max_connections: NonZeroUsize::new(DEFAULT_MAX_CONNECTIONS)
            .expect("the default cap is not zero"),
        beat: DEFAULT_BEAT,
        floor: duded::settings::floor_from_values(
            floor_assets,
            installed_floor,
            std::env::var_os("DUDES_FLOOR_PORT"),
        )?,
        hook_command: exe
            .as_ref()
            .map(|exe| exe.with_file_name(format!("dudes-hook{}", std::env::consts::EXE_SUFFIX))),
        daemon_exe: exe,
    };
    let options = startup.options;
    #[cfg(feature = "fault-injection")]
    let options = options.with_recovery_stop(recovery_stop);
    Ok((settings, operator.providers, memory, options))
}

/// The uid this daemon runs as, which an installed Floor under `share` must belong to. Windows,
/// which has no uid, checks no owner.
fn effective_uid() -> u32 {
    #[cfg(unix)]
    let uid = nix::unistd::geteuid().as_raw();
    #[cfg(not(unix))]
    let uid = 0;
    uid
}

fn state_dir_from_values(
    configured: Option<std::ffi::OsString>,
    root: Option<&std::path::Path>,
) -> anyhow::Result<PathBuf> {
    let directory =
        directory_from_values(configured, root, "DUDES_STATE_DIR", ".local/share/dudes")?;
    std::path::absolute(directory).map_err(Into::into)
}

fn storage_root_from_values(
    home: Option<std::ffi::OsString>,
    profile: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    home.or(profile)
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
}

fn directory_from_values(
    configured: Option<std::ffi::OsString>,
    root: Option<&std::path::Path>,
    name: &str,
    subdir: &str,
) -> anyhow::Result<PathBuf> {
    let directory = match configured {
        Some(value) => PathBuf::from(value),
        None => {
            root.filter(|root| root.is_absolute())
                .ok_or_else(|| {
                    if cfg!(windows) {
                        anyhow::anyhow!("HOME is unavailable; set an absolute HOME or, with HOME unset, an absolute USERPROFILE or {name}")
                    } else {
                        anyhow::anyhow!("HOME is unavailable; set an absolute HOME or {name}")
                    }
                })?
                .join(subdir)
        }
    };
    anyhow::ensure!(!directory.as_os_str().is_empty(), "{name} is empty");
    Ok(directory)
}

/// `env` plus the user database's name for this process's uid as `USER`, when `env` has no `USER`.
///
/// A login shell sets `USER`, so a daemon started from one passes it to every managed child.
/// Quickstart composes the daemon's environment without it, and the
/// Claude CLI finds its keychain login by that name: without it, every turn is "Not logged in".
/// The name comes from the daemon's own uid, never from whoever ran quickstart.
#[cfg(unix)]
fn with_user_name(mut env: Vec<EnvVar>) -> Vec<EnvVar> {
    if !env.iter().any(|var| var.name() == "USER")
        && let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::getuid())
    {
        env.push(EnvVar::new("USER", user.name));
    }
    env
}

fn operator_config_path() -> anyhow::Result<PathBuf> {
    Ok(duded::config::operator_config_path(|name| {
        std::env::var_os(name)
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that records each buffer it is handed and takes every one whole.
    #[derive(Default)]
    struct Calls(Vec<Vec<u8>>);

    impl io::Write for Calls {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.push(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_startup_line_is_escaped_and_written_in_one_call() {
        let mut sink = Calls::default();
        write_line(&mut sink, "duded: run\nduded: forged\u{202e}")
            .expect("this sink takes every buffer whole");
        assert_eq!(
            sink.0,
            [concat!(r"duded: run\u{a}duded: forged\u{202e}", "\n").as_bytes()],
            "one call: quickstart's spawn marker lands between two"
        );
    }

    #[test]
    fn storage_root_uses_profile_only_when_home_is_absent() {
        let home = tempfile::tempdir().expect("absolute home");
        let profile = tempfile::tempdir().expect("absolute profile");
        let profile_value = Some(profile.path().as_os_str().to_owned());
        assert_eq!(
            storage_root_from_values(None, profile_value.clone()),
            Some(profile.path().to_path_buf())
        );
        assert_eq!(
            storage_root_from_values(
                Some(home.path().as_os_str().to_owned()),
                profile_value.clone()
            ),
            Some(home.path().to_path_buf())
        );
        for bad in ["", "relative", "C:relative", r"\relative"] {
            assert_eq!(
                storage_root_from_values(Some(bad.into()), profile_value.clone()),
                None
            );
            assert_eq!(storage_root_from_values(None, Some(bad.into())), None);
        }
    }

    #[test]
    fn a_relative_state_directory_is_anchored_before_provider_launch() {
        let state = state_dir_from_values(Some("state".into()), None).expect("explicit state");
        assert_eq!(
            state,
            std::env::current_dir().expect("startup cwd").join("state")
        );
        assert!(state.join("agents/example/pi-sessions").is_absolute());
        assert!(
            state
                .join("agents/example/managed-settings.json")
                .is_absolute()
        );
        assert_eq!(
            directory_from_values(
                Some("relative-runtime".into()),
                None,
                "DUDES_RUNTIME_DIR",
                ".local/state/dudes"
            )
            .expect("explicit relative runtime"),
            PathBuf::from("relative-runtime")
        );
    }

    #[test]
    fn a_default_state_directory_never_borrows_cwd_as_home() {
        let home = tempfile::tempdir().expect("absolute home");
        let state = state_dir_from_values(None, Some(home.path()))
            .expect("absolute home supplies the default");
        assert_eq!(state, home.path().join(".local/share/dudes"));
        for absent_or_relative in [
            None,
            Some(std::path::Path::new("")),
            Some(std::path::Path::new("relative-home")),
        ] {
            assert!(
                state_dir_from_values(None, absent_or_relative).is_err(),
                "an unavailable home must refuse the default state directory"
            );
        }
    }

    #[cfg(not(feature = "stub-provider"))]
    fn registered(tables: ProviderTables) -> Vec<dudes_types::Provider> {
        let adapters =
            providers_table(&[], tables, std::path::Path::new("/state")).expect("a registry");
        dudes_types::Provider::ALL
            .into_iter()
            .filter(|provider| adapters.get(*provider).map(Adapter::provider) == Some(*provider))
            .collect()
    }

    #[cfg(not(feature = "stub-provider"))]
    #[test]
    fn each_provider_table_registers_its_adapter_and_no_table_registers_none() {
        use dudes_providers::codex::config::CodexConfig;
        use dudes_providers::pi::config::PiConfig;
        use dudes_types::Provider::{Claude, Codex, Pi};

        assert_eq!(registered(ProviderTables::default()), [Claude]);
        assert_eq!(
            registered(ProviderTables {
                pi: Some(PiConfig::default()),
                ..ProviderTables::default()
            }),
            [Claude, Pi]
        );
        assert_eq!(
            registered(ProviderTables {
                codex: Some(CodexConfig::default()),
                pi: Some(PiConfig::default()),
            }),
            [Claude, Codex, Pi]
        );
    }

    /// A `stub-provider` daemon that cannot find its stub does not start, and does not quietly run
    /// the real provider CLI instead.
    ///
    /// **This is the assertion the whole feature rests on.** [`ClaudeConfig::default`] names
    /// `claude`, so the failure mode here is not a crash but a silent success: a feature-on daemon
    /// whose variable was misspelled would launch the operator's real CLI and spend real vendor
    /// credit, which is the cost this feature exists to remove. An acceptance run that *could*
    /// spend credit is a failed run, so the refusal is what makes such a run trustworthy — not the
    /// intention behind it.
    ///
    /// The empty-string arm is separate on purpose: that is the shape a shell supplies when a
    /// variable is assigned from an unset one (`FOO=$BAR` with `BAR` unset), which is the likeliest
    /// way a gate script arms this wrongly while looking armed.
    #[cfg(feature = "stub-provider")]
    #[test]
    fn a_stub_build_without_its_stub_refuses_rather_than_running_the_real_cli() {
        let armed = configured_claude(&[EnvVar::new(STUB_COMMAND_VAR, "/nowhere/stub-agent")])
            .expect("an armed stub build configures its stub");
        assert_eq!(
            armed.command,
            std::ffi::OsString::from("/nowhere/stub-agent")
        );
        assert_ne!(
            armed.command,
            ClaudeConfig::default().command,
            "the stub build configured the real provider CLI"
        );

        for (label, env) in [
            ("unset", Vec::new()),
            ("empty", vec![EnvVar::new(STUB_COMMAND_VAR, "")]),
            (
                "another variable entirely",
                vec![EnvVar::new("DUDES_SOCK", "/tmp/sock")],
            ),
        ] {
            let refused = configured_claude(&env);
            assert!(
                refused.is_err(),
                "{label}: a stub build fell through to {:?}",
                refused.map(|config| config.command)
            );
        }
    }

    #[cfg(feature = "stub-provider")]
    #[test]
    fn a_stub_build_refuses_a_codex_or_pi_table_rather_than_running_a_real_cli() {
        use dudes_providers::codex::config::CodexConfig;
        use dudes_providers::pi::config::PiConfig;

        let stub = [EnvVar::new(STUB_COMMAND_VAR, "/nowhere/stub-agent")];
        let without = providers_table(
            &stub,
            ProviderTables::default(),
            std::path::Path::new("/state"),
        )
        .expect("an armed stub build starts");
        assert_eq!(
            dudes_types::Provider::ALL
                .into_iter()
                .filter(|provider| without.get(*provider).is_some())
                .collect::<Vec<_>>(),
            [dudes_types::Provider::Claude]
        );
        for (name, tables) in [
            (
                "codex",
                ProviderTables {
                    codex: Some(CodexConfig::default()),
                    ..ProviderTables::default()
                },
            ),
            (
                "pi",
                ProviderTables {
                    pi: Some(PiConfig::default()),
                    ..ProviderTables::default()
                },
            ),
        ] {
            let refused =
                providers_table(&stub, tables, std::path::Path::new("/state")).map(|_| ());
            assert!(
                refused
                    .as_ref()
                    .is_err_and(|error| error.to_string().contains(&format!("[providers.{name}]"))),
                "a stub build accepted [providers.{name}]: {refused:?}"
            );
        }
    }

    /// A token out of the seam is exactly what `HookToken` accepts, and two draws differ. Without
    /// the second arm a generator that returned a constant would pass.
    #[test]
    fn the_os_seam_mints_well_formed_distinct_tokens() {
        let mut rng = OsRng;
        let first = rng.agent_token();
        let second = rng.agent_token();
        assert_eq!(first.expose().len(), 43);
        assert!(
            first
                .expose()
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
            "base64url only: {}",
            first.expose()
        );
        assert_ne!(first, second, "two draws are two tokens");
        assert_ne!(rng.ulid(), rng.ulid());
    }

    /// The encoder is the standard alphabet, checked against a value computed by hand rather than
    /// against itself: 32 zero bytes are 43 `A`s, and flipping the last bit moves the last
    /// character.
    #[test]
    fn base64url_encodes_thirty_two_bytes_as_forty_three_characters() {
        let zeros = [0u8; 32];
        assert_eq!(base64url(&zeros), "A".repeat(43));
        let mut one = [0u8; 32];
        one[31] = 1;
        let encoded = base64url(&one);
        assert_eq!(encoded.len(), 43);
        assert_ne!(encoded, "A".repeat(43), "the last byte reaches the output");
    }

    /// The clock reads forward, and reads the wall clock rather than zero — the control being that
    /// the value is past a date this code was written after.
    #[test]
    fn the_system_clock_reads_the_wall_clock() {
        let now = SystemClock.now();
        assert!(
            now > Timestamp::from_unix_millis(1_700_000_000_000),
            "the clock read {now:?}, which is before this was written"
        );
    }

    /// A `USER` the daemon started with is the one its children get, as before. The absent arm is
    /// `bins/dudes/tests/quickstart.rs`'s, through a real quickstart.
    #[cfg(unix)]
    #[test]
    fn a_user_the_daemon_started_with_is_kept() {
        use std::ffi::OsStr;

        let env = with_user_name(vec![EnvVar::new("USER", "someone-else")]);
        let pairs: Vec<_> = env
            .iter()
            .map(|var| (var.name(), var.expose_value()))
            .collect();
        assert_eq!(pairs, [(OsStr::new("USER"), OsStr::new("someone-else"))]);
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_host_relation_mapping_is_exhaustive() {
        use duded::classify::Relation;
        use dudes_host::PeerRelation;

        assert_eq!(relation_of(PeerRelation::Matched(3)), Relation::Matched(3));
        assert_eq!(relation_of(PeerRelation::Pending(7)), Relation::Pending(7));
        assert_eq!(relation_of(PeerRelation::NoMatch), Relation::NoMatch);
        assert_eq!(
            relation_of(PeerRelation::Unresolvable),
            Relation::Unresolvable
        );
        assert_eq!(relation_of(PeerRelation::Abandoned), Relation::Abandoned);
    }
}
