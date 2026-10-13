//! Where a managed child's standard output ends up: the daemon's side of the host's stream seam.
//!
//! # What this is for
//!
//! A managed turn's result — its outcome, its usage, the model's last message, the session the
//! vendor confirmed — exists only in the lines the child writes to its standard output. The host
//! owns the pipe and the reap; the provider adapter owns the parsing; **this owns the join**: it
//! is the one place that knows a pid belongs to a `TurnId`, and it is the only production
//! constructor of `Input::ChildExited`.
//!
//! # Why the correlation is a key and not a pid
//!
//! A pid alone is not an identity. The host reaps a child the instant it exits, which releases the
//! identifier, and a later spawn may be given the same number — so a message about the first child
//! arriving after the second was registered would end the wrong turn. The whole adoption key is
//! compared instead, field by field: a different process at the same identifier has a different
//! start value, and a recording from another boot has a different boot identity.
//!
//! That is the same question `dudes-host`'s own `names_the_owned_child` asks and is answered the
//! same way. It is **not** the key-against-a-probe comparison reserved for
//! `liveness()`: both keys here were recorded by this host, in this run, and no host state takes
//! part.
//!
//! # Nothing here decides anything
//!
//! An `Input` is posted and the supervisor rules on it. In particular a `ChildExited` for a turn
//! the reaper has already ended is refused by `Supervisor::end_turn`, which is the single place
//! that says a turn has exactly one end — so a child found `Gone` by a probe and reaped by its own
//! watcher in the same beat produces one `TurnEnded`, not two.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use dudes_proto::{ExitStatus, LiveActivityKind};
use dudes_providers::codex::startup::CodexStartup;
use dudes_providers::launch::{LaunchInput, LaunchSpec};
use dudes_providers::protocol::ProtocolAction;
use dudes_providers::stream::{LineRead, SessionObservation, TurnEnd, TurnReader};
use dudes_supervisor::Input;
use dudes_supervisor::host::AdoptionKey;
use dudes_types::{Lsn, TurnId};

use crate::output::OutputRegistry;
use crate::ports::Handle;

type PendingEnds = BTreeMap<(u32, u64, Option<String>), PendingEnd>;
type ActivityKey = (u32, u64, Option<String>);
type PendingObservations = BTreeMap<ActivityKey, Vec<(Option<Lsn>, SessionObservation)>>;

/// The daemon's stream sink: one per run, shared by the loop and by every child's watcher thread.
///
/// Built before the loop exists — the host is constructed first and must be handed somewhere to
/// send lines — so the way back into the loop arrives later, through [`ChildEnds::arm`]. Until it
/// does, an end is logged and dropped rather than lost silently, which is the right behaviour for
/// the only window in which one can happen: a child that outlived a daemon that never finished
/// starting.
#[derive(Debug, Default)]
pub struct ChildEnds {
    handle: OnceLock<Handle>,
    open: Mutex<BTreeMap<u32, Registered>>,
    uncertain: Mutex<UncertainChildren>,
    next_reader: Mutex<Option<Box<dyn TurnReader>>>,
    next_startup: Mutex<Option<CodexStartup>>,
    next_outbox: Mutex<Option<Arc<crate::mailbox::OutboxGrant>>>,
    next_mcp_grant: Mutex<Option<Lsn>>,
    codex_profile: OnceLock<Arc<crate::codex_profile::CodexProfile>>,
    protocols: Mutex<BTreeMap<ActivityKey, bool>>,
    pending_observation: Mutex<PendingObservations>,
    sampling: AtomicBool,
    pending_activity: Mutex<BTreeMap<ActivityKey, LiveActivityKind>>,
    pending_end: Mutex<PendingEnds>,
    output: Arc<OutputRegistry>,
}

#[derive(Debug, Default)]
struct UncertainChildren {
    launches: BTreeMap<TurnId, std::path::PathBuf>,
    overflow: bool,
}

/// One spawned child, as the loop registered it.
#[derive(Debug)]
struct Registered {
    /// The rest of the adoption key, so a recycled identifier cannot answer for this turn.
    start_time: u64,
    boot: Option<String>,
    turn: TurnId,
    mcp_grant: Option<Lsn>,
    cwd: Option<std::path::PathBuf>,
}

/// An owned child that exited before `Effect::Spawn` could register its full key.
/// The vendor's final message must never appear in a diagnostic formatter.
struct PendingEnd {
    mcp_grant: Option<Lsn>,
    elapsed_ms: Option<u64>,
    exit: ExitStatus,
    end: TurnEnd,
}

impl std::fmt::Debug for PendingEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingEnd")
            .field("exit", &self.exit)
            .finish_non_exhaustive()
    }
}

const MAX_PENDING_ENDS: usize = 128;
const MAX_PROTOCOL_CHILDREN: usize = 128;

impl ChildEnds {
    /// A sink with no way back into the loop yet.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// This daemon run's typed, bounded output read seam.
    #[must_use]
    pub fn output(&self) -> Arc<OutputRegistry> {
        Arc::clone(&self.output)
    }

    /// Give the sink the way back into the decide loop. Called once, by `boot`, as soon as the
    /// loop's queue exists.
    pub fn arm(&self, handle: Handle) {
        let _ = self.handle.set(handle);
    }

    pub(crate) fn configure_codex_profile(&self, profile: Arc<crate::codex_profile::CodexProfile>) {
        let _ = self.codex_profile.set(profile);
    }

    pub(crate) fn prepare_launch(
        &self,
        spec: &mut LaunchSpec,
        outbox: Option<Arc<crate::mailbox::OutboxGrant>>,
    ) -> std::io::Result<()> {
        if let Some(outbox) = &outbox {
            outbox.check()?;
            if !matches!(spec.stdin, LaunchInput::Protocol(_)) {
                if spec.argv.get(1).is_none_or(|part| part != "exec")
                    || spec.argv.get(2).is_none_or(|part| part != "--add-dir")
                    || spec.argv.get(3).is_none()
                {
                    return Err(std::io::Error::other("Codex outbox option is unavailable"));
                }
                outbox.path().as_os_str().clone_into(&mut spec.argv[3]);
            }
        }
        if let LaunchInput::Protocol(startup) = &spec.stdin {
            let startup = startup.clone();
            let profile = self
                .codex_profile
                .get()
                .ok_or_else(|| std::io::Error::other("Codex streaming profile is unavailable"))?;
            let prepared = profile.prepare(spec, &startup)?;
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if outbox
                .as_ref()
                .is_none_or(|outbox| prepared.agent_dir().join("outbox") != outbox.path())
            {
                return Err(std::io::Error::other(
                    "Codex outbox does not belong to this agent",
                ));
            }
            spec.stdin = LaunchInput::Protocol(prepared);
        }
        *self
            .next_outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = outbox;
        Ok(())
    }

    pub(crate) fn read_next_for(
        &self,
        reader: Option<Box<dyn TurnReader>>,
        spec: &LaunchSpec,
        mcp_grant: Option<Lsn>,
    ) {
        self.read_next_with(reader);
        *self
            .next_mcp_grant
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = mcp_grant;
        *self
            .next_startup
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = match &spec.stdin {
            LaunchInput::Protocol(startup) => Some(startup.clone()),
            _ => None,
        };
    }

    /// Arm one owned protocol child without admitting another task to the process.
    pub fn watch_protocol(
        self: &Arc<Self>,
        key: &AdoptionKey,
        input: Box<dyn ChildInput>,
    ) -> ChildTurn {
        let mut child = self.watch(Some(key));
        child.input = Some(input);
        let mut protocols = self
            .protocols
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if protocols.len() >= MAX_PROTOCOL_CHILDREN || child.startup.is_none() {
            drop(protocols);
            child.failed = true;
            if let Some(handle) = self.handle.get() {
                handle.protocol_failed(key.clone());
            }
            if let Some(input) = &child.input {
                let _ = input.send(ProtocolAction::Fail);
            }
            return child;
        }
        protocols.insert(protocol_key(key), false);
        drop(protocols);
        let actions = child.reader.start();
        child.actions(actions);
        child
    }

    /// A transport failure belongs only to the full key that the host still owns.
    pub fn protocol_failed(&self, key: &AdoptionKey) {
        let mut protocols = self
            .protocols
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let first = protocols
            .get_mut(&protocol_key(key))
            .is_some_and(|failed| !std::mem::replace(failed, true));
        drop(protocols);
        if first && let Some(handle) = self.handle.get() {
            handle.protocol_failed(key.clone());
        }
    }

    /// Remember which turn the child `key` names serves.
    ///
    /// Called by the executor on the `Effect::Spawn` the host answered, which is the only moment
    /// the two facts — the turn and the key — are in the same hand.
    pub fn spawned(&self, turn: TurnId, key: &AdoptionKey, mcp_grant: Option<Lsn>) {
        self.register_spawn(turn, key, mcp_grant, None);
    }

    pub(crate) fn spawned_in(
        &self,
        turn: TurnId,
        key: &AdoptionKey,
        mcp_grant: Option<Lsn>,
        cwd: &std::path::Path,
    ) {
        self.register_spawn(turn, key, mcp_grant, Some(cwd.to_path_buf()));
    }

    pub(crate) fn visit_owned<E>(
        &self,
        mut visit: impl FnMut(TurnId, AdoptionKey, Option<&std::path::Path>) -> Result<(), E>,
    ) -> Result<(), E> {
        for (pid, child) in self.open().iter() {
            visit(
                child.turn,
                AdoptionKey {
                    pid: *pid,
                    start_time: child.start_time,
                    boot: child.boot.clone(),
                },
                child.cwd.as_deref(),
            )?;
        }
        Ok(())
    }

    pub(crate) fn retain_uncertain(&self, turn: TurnId, cwd: &std::path::Path) {
        let mut uncertain = self
            .uncertain
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if uncertain.launches.len() < MAX_PENDING_ENDS {
            uncertain.launches.insert(turn, cwd.to_path_buf());
        } else {
            uncertain.overflow = true;
        }
    }

    pub(crate) fn visit_uncertain<E>(
        &self,
        mut visit: impl FnMut(TurnId, &std::path::Path) -> Result<(), E>,
    ) -> Result<bool, E> {
        let uncertain = self
            .uncertain
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (turn, cwd) in &uncertain.launches {
            visit(*turn, cwd)?;
        }
        Ok(uncertain.overflow)
    }

    fn register_spawn(
        &self,
        turn: TurnId,
        key: &AdoptionKey,
        mcp_grant: Option<Lsn>,
        cwd: Option<std::path::PathBuf>,
    ) {
        let mut open = self.open();
        open.insert(
            key.pid,
            Registered {
                start_time: key.start_time,
                boot: key.boot.clone(),
                turn,
                mcp_grant,
                cwd,
            },
        );
        for (observed_grant, observation) in self.take_pending_observation(key) {
            if observed_grant == mcp_grant {
                self.post_observation(turn, key, mcp_grant, observation);
            }
        }
        if let Some(pending) = self.take_pending_end(key)
            && pending.mcp_grant == mcp_grant
        {
            open.remove(&key.pid);
            self.post_end(
                turn,
                key,
                mcp_grant,
                pending.elapsed_ms,
                pending.exit,
                pending.end,
            );
        }
        // `finish` stores an early end under this same lock. Release only after posting
        // init then end, so their accepted durable records retain that order.
        drop(open);
    }

    /// The reader for the child the host spawns next, or `None` when no adapter can read it.
    ///
    /// The executor calls this immediately before it asks the host to spawn, and the host calls
    /// [`Self::watch`] synchronously inside that spawn, so the next `watch` takes this reader.
    pub fn read_next_with(&self, reader: Option<Box<dyn TurnReader>>) {
        *self
            .next_startup
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        *self
            .next_reader
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = reader;
    }

    /// A consumer for one child's standard output, read with the reader
    /// [`Self::read_next_with`] armed, or with [`Unread`] when none was armed so the turn still
    /// ends.
    ///
    /// `key` is `None` when the host could not read one after the exec. The stream is still read —
    /// the child must not block on a pipe nobody drains — and what it collects is dropped at the
    /// end, because nothing can say which turn it belonged to.
    #[must_use]
    pub fn watch(self: &Arc<Self>, key: Option<&AdoptionKey>) -> ChildTurn {
        let armed = self
            .next_reader
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let mut child = self.watch_with(key, armed.unwrap_or_else(|| Box::new(Unread::default())));
        child.startup = self
            .next_startup
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        child.outbox = self
            .next_outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        child.mcp_grant = self
            .next_mcp_grant
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        child
    }

    /// [`Self::watch`] with an explicit reader and no armed one, for a consumer the loop did not
    /// spawn.
    #[must_use]
    pub fn watch_with(
        self: &Arc<Self>,
        key: Option<&AdoptionKey>,
        reader: Box<dyn TurnReader>,
    ) -> ChildTurn {
        ChildTurn {
            ends: Arc::clone(self),
            key: key.cloned(),
            reader,
            startup: None,
            outbox: None,
            input: None,
            failed: false,
            mcp_grant: None,
        }
    }

    fn observed_session(
        &self,
        key: &AdoptionKey,
        mcp_grant: Option<Lsn>,
        observation: SessionObservation,
    ) {
        let open = self.open();
        let turn = open.get(&key.pid).and_then(|registered| {
            (registered.start_time == key.start_time
                && registered.boot.as_deref() == key.boot.as_deref())
            .then_some(registered.turn)
        });
        if let Some(turn) = turn {
            if open
                .get(&key.pid)
                .is_some_and(|registered| registered.mcp_grant != mcp_grant)
            {
                return;
            }
            drop(open);
            self.post_observation(turn, key, mcp_grant, observation);
            return;
        }
        self.pending_observation()
            .entry(protocol_key(key))
            .or_default()
            .push((mcp_grant, observation));
    }

    fn post_observation(
        &self,
        turn: TurnId,
        key: &AdoptionKey,
        mcp_grant: Option<Lsn>,
        observation: SessionObservation,
    ) {
        let Some(handle) = self.handle.get() else {
            tracing::error!(
                ?turn,
                "a managed session opened before the loop was reachable"
            );
            return;
        };
        handle.input(Input::SessionObserved {
            turn,
            key: key.clone(),
            mcp_grant,
            observation,
        });
    }

    /// Keep structured program activity for [`Self::flush_activities`]. The daemon calls this
    /// only when it serves the Floor; until then program lines leave no pending kind.
    pub fn sample_activity(&self) {
        self.sampling.store(true, Ordering::Relaxed);
    }

    fn observed_activity(&self, key: &AdoptionKey, kind: LiveActivityKind) {
        const MAX_PENDING_ACTIVITY: usize = 128;
        if !self.sampling.load(Ordering::Relaxed) {
            return;
        }
        let mut pending = self
            .pending_activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let parts = (key.pid, key.start_time, key.boot.clone());
        if !pending.contains_key(&parts) && pending.len() == MAX_PENDING_ACTIVITY {
            pending.pop_first();
        }
        pending.insert(parts, kind);
    }

    /// The turn's end replaces whatever kind its last lines left.
    fn forget_activity(&self, key: &AdoptionKey) {
        self.pending_activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(key.pid, key.start_time, key.boot.clone()));
    }

    fn post_activity(&self, turn: TurnId, kind: LiveActivityKind) {
        if let Some(handle) = self.handle.get() {
            handle.input(Input::ActivityObserved { turn, kind });
        }
    }

    /// Post at most the newest fixed kind for each currently registered child.
    pub fn flush_activities(&self) {
        let open = self.open();
        let mut pending = self
            .pending_activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let ready: Vec<_> = pending
            .iter()
            .filter_map(|((pid, start, boot), kind)| {
                open.get(pid)
                    .filter(|registered| {
                        registered.start_time == *start && registered.boot == *boot
                    })
                    .map(|registered| ((*pid, *start, boot.clone()), registered.turn, *kind))
            })
            .collect();
        for (key, turn, kind) in ready {
            pending.remove(&key);
            self.post_activity(turn, kind);
        }
    }

    /// The turn the child `key` names serves, taken off the register.
    ///
    /// Presence before value, and every field compared: see this module's own docs for why a pid
    /// is not enough and why this is not the key-against-a-probe comparison.
    #[cfg(test)]
    fn claim(&self, key: &AdoptionKey) -> Option<TurnId> {
        let mut open = self.open();
        let registered = open.get(&key.pid)?;
        if registered.start_time != key.start_time
            || registered.boot.as_deref() != key.boot.as_deref()
        {
            return None;
        }
        open.remove(&key.pid).map(|registered| registered.turn)
    }

    /// Post one managed turn's end into the loop.
    fn finish(
        &self,
        key: &AdoptionKey,
        mcp_grant: Option<Lsn>,
        elapsed_ms: Option<u64>,
        exit: ExitStatus,
        end: TurnEnd,
    ) {
        let mut open = self.open();
        let turn = open.get(&key.pid).and_then(|registered| {
            (registered.start_time == key.start_time
                && registered.boot.as_deref() == key.boot.as_deref())
            .then_some(registered.turn)
        });
        let Some(turn) = turn else {
            // Hold the `open` lock through this insertion. A concurrent `spawned`
            // either precedes this check or sees the pending end; no gap can lose it.
            let mut pending = self.pending_end();
            let parts = (key.pid, key.start_time, key.boot.clone());
            if !pending.contains_key(&parts) && pending.len() == MAX_PENDING_ENDS {
                pending.pop_first();
                tracing::warn!("pre-registration child end capacity was exceeded");
            }
            pending.entry(parts).or_insert(PendingEnd {
                mcp_grant,
                elapsed_ms,
                exit,
                end,
            });
            return;
        };
        if open
            .get(&key.pid)
            .is_some_and(|registered| registered.mcp_grant != mcp_grant)
        {
            return;
        }
        open.remove(&key.pid);
        drop(open);
        self.post_end(turn, key, mcp_grant, elapsed_ms, exit, end);
    }

    fn post_end(
        &self,
        turn: TurnId,
        key: &AdoptionKey,
        mcp_grant: Option<Lsn>,
        elapsed_ms: Option<u64>,
        exit: ExitStatus,
        mut end: TurnEnd,
    ) {
        if let Some(observation) = end.final_observation.take() {
            self.post_observation(turn, key, mcp_grant, observation);
        }
        if let Some(refused) = &end.usage_refused {
            // Invariant 4 rather than a rounding: the vendor reported a cost that cannot be
            // charged, so none is. The turn still ends, and what the budget does with an end it
            // cannot price is the budget's own business.
            tracing::warn!(%refused, ?turn, "a managed turn's cost could not be charged");
        }
        if end.attribution_refused > 0 {
            tracing::warn!(
                refused = end.attribution_refused,
                ?turn,
                "result lines could not be attributed to this managed turn"
            );
        }
        if end.unreadable > 0 {
            tracing::warn!(
                unreadable = end.unreadable,
                ?turn,
                "lines of this turn's stream could not be read; a vendor payload may have drifted"
            );
        }
        let Some(handle) = self.handle.get() else {
            tracing::error!(?turn, "a managed turn ended before the loop was reachable");
            return;
        };
        handle.input(Input::ChildExited {
            final_reply: end.final_reply.map(|reply| dudes_proto::ChatReply {
                text: reply.text,
                completeness: match reply.completeness {
                    dudes_providers::stream::FinalReplyCompleteness::Complete => {
                        dudes_proto::ChatReplyCompleteness::Complete
                    }
                    dudes_providers::stream::FinalReplyCompleteness::Truncated => {
                        dudes_proto::ChatReplyCompleteness::Truncated
                    }
                },
            }),
            elapsed_ms,
            turn,
            exit,
            usage: end.usage,
            model_usage: end.model_usage,
            outcome: end.outcome,
            last_message: end.last_message,
        });
    }

    /// The register, with a poisoned lock read through rather than panicked on: a watcher thread
    /// that panicked must not stop every other turn from ending.
    fn open(&self) -> std::sync::MutexGuard<'_, BTreeMap<u32, Registered>> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pending_observation(&self) -> std::sync::MutexGuard<'_, PendingObservations> {
        self.pending_observation
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn pending_end(&self) -> std::sync::MutexGuard<'_, PendingEnds> {
        self.pending_end
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn take_pending_end(&self, key: &AdoptionKey) -> Option<PendingEnd> {
        self.pending_end()
            .remove(&(key.pid, key.start_time, key.boot.clone()))
    }

    fn take_pending_observation(
        &self,
        key: &AdoptionKey,
    ) -> Vec<(Option<Lsn>, SessionObservation)> {
        self.pending_observation()
            .remove(&protocol_key(key))
            .unwrap_or_default()
    }
}

/// The daemon's neutral handle to a host-owned bounded protocol writer.
pub trait ChildInput: Send + std::fmt::Debug {
    /// Queue an action without blocking the reader on a pipe write.
    fn send(&self, action: ProtocolAction) -> std::io::Result<()>;
}

fn protocol_key(key: &AdoptionKey) -> ActivityKey {
    (key.pid, key.start_time, key.boot.clone())
}

/// One managed child's standard output, as far as it has been read.
///
/// Not `Clone` and consumed by [`ChildTurn::ended`], so a turn cannot be ended twice from here.
#[derive(Debug)]
pub struct ChildTurn {
    ends: Arc<ChildEnds>,
    key: Option<AdoptionKey>,
    reader: Box<dyn TurnReader>,
    startup: Option<CodexStartup>,
    outbox: Option<Arc<crate::mailbox::OutboxGrant>>,
    input: Option<Box<dyn ChildInput>>,
    failed: bool,
    mcp_grant: Option<Lsn>,
}

impl ChildTurn {
    /// One line of the child's standard output, with its newline removed.
    pub fn line(&mut self, text: &str) {
        let read = self.reader.line(text);
        let Some(key) = &self.key else {
            return;
        };
        for block in read.text {
            self.ends
                .output
                .capture(key, block.session.as_str(), &block.text);
        }
        if let Some(session) = read.omitted_text {
            self.ends.output.omit(key, session.as_str());
        }
        if let Some(observation) = read.session {
            self.ends.observed_session(key, self.mcp_grant, observation);
        }
        if let Some(kind) = read.activity {
            self.ends.observed_activity(key, kind);
        }
        self.actions(read.actions);
    }

    fn actions(&mut self, actions: Vec<ProtocolAction>) {
        for action in actions {
            let Some(input) = &self.input else {
                return;
            };
            let admitted = match &action {
                ProtocolAction::Write(frame) if frame.is_task() => {
                    let profile_admitted = self
                        .startup
                        .as_ref()
                        .zip(self.ends.codex_profile.get())
                        .is_some_and(|(startup, profile)| profile.before_task(startup).is_ok());
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    let outbox_admitted = self
                        .outbox
                        .as_ref()
                        .is_some_and(|outbox| outbox.check().is_ok());
                    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
                    let outbox_admitted = true;
                    profile_admitted
                        && outbox_admitted
                        && self
                            .ends
                            .handle
                            .get()
                            .is_some_and(|handle| handle.require_journal_writes().is_ok())
                }
                _ => true,
            };
            if !admitted || input.send(action).is_err() {
                self.failed = true;
                if let Some(key) = &self.key {
                    self.ends.protocol_failed(key);
                }
                let _ = input.send(ProtocolAction::Fail);
                break;
            }
        }
    }

    /// The stream is at its end and the child has been reaped.
    pub fn ended(self, exit: ExitStatus) {
        self.ended_with_elapsed(exit, None);
    }

    /// End the stream with duration observed by the host at confirmed reap.
    pub fn ended_with_elapsed(self, exit: ExitStatus, elapsed_ms: Option<u64>) {
        let Self {
            ends,
            key,
            reader,
            failed,
            mcp_grant,
            ..
        } = self;
        let Some(key) = key else {
            tracing::warn!(
                "a managed child whose adoption key could not be read has ended; its stream is \
                 dropped because nothing can say which turn it served"
            );
            return;
        };
        ends.output.watch_finished(&key);
        ends.forget_activity(&key);
        let protocol_failed = ends
            .protocols
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&protocol_key(&key))
            .unwrap_or(false);
        let mut end = reader.end(exit);
        if failed || protocol_failed {
            if !matches!(end.outcome, dudes_proto::TurnOutcome::Failed { .. }) {
                end.outcome = dudes_proto::TurnOutcome::Failed {
                    reason: "Codex streaming transport failed".into(),
                };
            }
            end.final_reply = None;
            end.last_message = None;
        }
        ends.finish(&key, mcp_grant, elapsed_ms, exit, end);
    }
}

/// The reader of a child no adapter was armed for: every line is unreadable and the turn ends
/// with no result, so its end still reaches the loop.
#[derive(Debug, Default)]
struct Unread {
    lines: u64,
}

impl TurnReader for Unread {
    fn line(&mut self, _text: &str) -> LineRead {
        self.lines += 1;
        LineRead::default()
    }

    fn end(self: Box<Self>, _exit: ExitStatus) -> TurnEnd {
        TurnEnd {
            final_observation: None,
            final_reply: None,
            outcome: dudes_proto::TurnOutcome::NoResult,
            usage: None,
            model_usage: BTreeMap::new(),
            last_message: None,
            usage_refused: None,
            session: None,
            attribution_refused: 0,
            unreadable: self.lines,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::ports::Job;
    use dudes_providers::stream::ScopedUsage;
    use dudes_types::{SessionId, TurnId, Usage, Usd};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record as SpanRecord};
    use tracing::{Event, Level, Metadata, Subscriber};

    use super::*;

    const SESSION: &str = "3f9a1c2e-0000-4000-8000-000000000001";
    const ATTRIBUTION_WARNING: &str = "result lines could not be attributed to this managed turn";

    #[derive(Default)]
    struct Capture(Arc<Mutex<Vec<(Level, String)>>>);

    #[derive(Default)]
    struct Message(String);

    impl Visit for Message {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    impl Subscriber for Capture {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _: &Id, _: &SpanRecord<'_>) {}

        fn record_follows_from(&self, _: &Id, _: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut message = Message::default();
            event.record(&mut message);
            self.0
                .lock()
                .unwrap()
                .push((*event.metadata().level(), message.0));
        }

        fn enter(&self, _: &Id) {}

        fn exit(&self, _: &Id) {}
    }

    fn key(pid: u32, start_time: u64) -> AdoptionKey {
        AdoptionKey {
            pid,
            start_time,
            boot: None,
        }
    }

    fn turn() -> TurnId {
        TurnId::from(ulid::Ulid::from_parts(1, 2))
    }

    const OPEN: &str = "open";

    #[test]
    fn observations_keep_the_daemons_grant_and_full_key_before_and_after_registration() {
        for registered_first in [true, false] {
            let ends = ChildEnds::new();
            let (sender, jobs) = std::sync::mpsc::channel();
            ends.arm(Handle::new(sender));
            let owned = AdoptionKey {
                boot: Some("owned-boot".into()),
                ..key(4242, 900)
            };
            let grant = Some(Lsn(11));
            if registered_first {
                ends.spawned(turn(), &owned, grant);
            }
            ends.observed_session(
                &owned,
                Some(Lsn(10)),
                SessionObservation {
                    servers: vec!["stale".into()],
                    ..SessionObservation::default()
                },
            );
            let opening = SessionObservation {
                session: Some(SessionId::new(SESSION)),
                credential: Some(dudes_providers::stream::Credential::NoApiKey),
                connected: Some(vec!["a".into()]),
                ..SessionObservation::default()
            };
            let incremental = SessionObservation {
                servers: vec!["rogue".into()],
                plugins: vec![Some("extra".into())],
                ..SessionObservation::default()
            };
            ends.observed_session(&owned, grant, opening.clone());
            ends.observed_session(&owned, grant, incremental.clone());
            if !registered_first {
                ends.spawned(turn(), &owned, grant);
            }
            for expected in [opening, incremental] {
                let Job::Input(input) = jobs.try_recv().unwrap() else {
                    panic!("an observation input");
                };
                let Input::SessionObserved {
                    turn: actual,
                    key,
                    mcp_grant,
                    observation,
                } = *input
                else {
                    panic!("an observation");
                };
                assert_eq!(actual, turn());
                assert_eq!(
                    (key.pid, key.start_time, key.boot),
                    (4242, 900, Some("owned-boot".into()))
                );
                assert_eq!(mcp_grant, Some(Lsn(11)));
                assert_eq!(observation, expected);
            }
            assert!(jobs.try_recv().is_err());
            assert!(ends.pending_observation().is_empty());
        }
    }

    #[derive(Debug, Default)]
    struct Scripted {
        attribution_refused: u64,
    }

    impl TurnReader for Scripted {
        fn line(&mut self, text: &str) -> LineRead {
            LineRead {
                actions: Vec::new(),
                omitted_text: None,
                text: Vec::new(),
                session: (text == OPEN).then(observation),
                activity: None,
            }
        }

        fn end(self: Box<Self>, _exit: ExitStatus) -> TurnEnd {
            TurnEnd {
                final_observation: None,
                final_reply: None,
                outcome: dudes_proto::TurnOutcome::Completed,
                usage: Some(ScopedUsage::Turn(Usage {
                    usd: Some(Usd::from_micros(250_000)),
                    ..Usage::ZERO
                })),
                model_usage: BTreeMap::new(),
                last_message: Some("done".to_owned()),
                usage_refused: None,
                session: Some(SessionId::new(SESSION)),
                attribution_refused: self.attribution_refused,
                unreadable: 0,
            }
        }
    }

    fn observation() -> SessionObservation {
        SessionObservation {
            session: Some(SessionId::new(SESSION)),
            servers: vec!["repository-tools".to_owned()],
            ..SessionObservation::default()
        }
    }

    const EXIT_0: ExitStatus = ExitStatus {
        code: Some(0),
        signal: None,
    };

    fn attribution_warnings(attribution_refused: u64) -> usize {
        let capture = Capture::default();
        let events = Arc::clone(&capture.0);
        tracing::subscriber::with_default(capture, || {
            let ends = ChildEnds::new();
            let child_key = key(4242, 900);
            ends.spawned(turn(), &child_key, None);
            ends.read_next_with(Some(Box::new(Scripted {
                attribution_refused,
            })));
            ends.watch(Some(&child_key)).ended(EXIT_0);
        });
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|(level, message)| *level == Level::WARN && message == ATTRIBUTION_WARNING)
            .count()
    }

    fn assert_observed(job: &Job) {
        assert!(matches!(
            job,
            Job::Input(input) if matches!(
                input.as_ref(),
                Input::SessionObserved { turn: observed, observation: posted, .. }
                    if *observed == turn() && *posted == observation()
            )
        ));
    }

    fn assert_end(job: &Job) {
        assert!(matches!(
            job,
            Job::Input(input) if matches!(
                input.as_ref(),
                Input::ChildExited {
                    exit,
                    usage: Some(ScopedUsage::Turn(usage)),
                    outcome: dudes_proto::TurnOutcome::Completed,
                    last_message: Some(last_message),
                    ..
                } if *exit == EXIT_0
                    && usage.usd == Some(Usd::from_micros(250_000))
                    && last_message == "done"
            )
        ));
    }

    #[test]
    fn a_session_observation_reaches_its_turn_for_both_registration_orders() {
        for registration_first in [true, false] {
            let ends = ChildEnds::new();
            let (sender, jobs) = std::sync::mpsc::channel();
            ends.arm(Handle::new(sender));
            let child_key = key(4242, 900);
            ends.read_next_with(Some(Box::new(Scripted::default())));
            let mut child = ends.watch(Some(&child_key));
            if registration_first {
                ends.spawned(turn(), &child_key, None);
            }

            child.line(OPEN);
            if !registration_first {
                assert!(
                    jobs.try_recv().is_err(),
                    "the observation waits only until its turn registration arrives"
                );
                ends.spawned(turn(), &child_key, None);
            }
            assert_observed(&jobs.try_recv().expect("the observation is posted live"));
            child.line("anything else");
            assert!(
                jobs.try_recv().is_err(),
                "a line that observes nothing posts nothing"
            );
            child.ended(EXIT_0);

            assert_end(
                &jobs
                    .try_recv()
                    .expect("the child end follows the observation"),
            );
            assert!(jobs.try_recv().is_err(), "exactly two inputs were posted");
        }
    }

    #[test]
    fn every_pre_registration_observation_is_posted_in_order_before_the_end() {
        let ends = ChildEnds::new();
        let (sender, jobs) = std::sync::mpsc::channel();
        ends.arm(Handle::new(sender));
        let child_key = key(4242, 900);
        let opening = SessionObservation {
            session: Some(SessionId::new(SESSION)),
            credential: Some(dudes_providers::stream::Credential::NoApiKey),
            connected: Some(Vec::new()),
            ..SessionObservation::default()
        };
        let rogue = SessionObservation {
            servers: vec!["rogue".to_owned()],
            plugins: vec![Some("unexpected-plugin".to_owned())],
            ..SessionObservation::default()
        };
        ends.observed_session(&child_key, None, opening.clone());
        ends.observed_session(&child_key, None, rogue.clone());
        ends.finish(
            &child_key,
            None,
            None,
            EXIT_0,
            Box::new(Scripted::default()).end(EXIT_0),
        );
        assert!(jobs.try_recv().is_err());
        ends.spawned(turn(), &child_key, None);
        for expected in [opening, rogue] {
            let Job::Input(input) = jobs.try_recv().expect("an observation") else {
                panic!("an input");
            };
            let Input::SessionObserved {
                turn: posted,
                observation,
                ..
            } = *input
            else {
                panic!("all observations precede the child end");
            };
            assert_eq!(posted, turn());
            assert_eq!(observation, expected);
        }
        assert_end(&jobs.try_recv().expect("the end follows both observations"));
        assert!(jobs.try_recv().is_err());
        assert!(ends.pending_observation().is_empty());
    }

    #[test]
    fn final_observation_precedes_exit_even_when_the_child_ends_before_registration() {
        for registration_first in [true, false] {
            let ends = ChildEnds::new();
            let (sender, jobs) = std::sync::mpsc::channel();
            ends.arm(Handle::new(sender));
            let child_key = key(4242, 900);
            ends.read_next_with(Some(Box::new(
                dudes_providers::pi::stream::TurnStream::new(),
            )));
            let mut child = ends.watch(Some(&child_key));
            if registration_first {
                ends.spawned(turn(), &child_key, None);
            }
            child.line(&format!(r#"{{"type":"session","id":"{SESSION}"}}"#));
            child.line(
                r#"{"type":"tool_execution_start","toolCallId":"call","toolName":"mcp__rogue__x"}"#,
            );
            child.line(r#"{"type":"tool_execution_end","toolCallId":"call","toolName":"mcp__rogue__x","isError":false}"#);
            child.ended(EXIT_0);
            if !registration_first {
                assert!(jobs.try_recv().is_err());
                ends.spawned(turn(), &child_key, None);
            }
            assert!(
                matches!(jobs.try_recv().unwrap(), Job::Input(input) if matches!(input.as_ref(), Input::SessionObserved { observation, .. } if observation.session.is_some()))
            );
            assert!(
                matches!(jobs.try_recv().unwrap(), Job::Input(input) if matches!(input.as_ref(), Input::SessionObserved { turn: actual, observation, .. } if *actual == turn() && observation.servers == ["rogue"]))
            );
            assert!(
                matches!(jobs.try_recv().unwrap(), Job::Input(input) if matches!(input.as_ref(), Input::ChildExited { turn: actual, outcome: dudes_proto::TurnOutcome::NoResult, .. } if *actual == turn()))
            );
            assert!(jobs.try_recv().is_err());
        }
    }

    #[test]
    fn pending_observations_keep_recycled_identifiers_and_boots_separate() {
        let current = AdoptionKey {
            boot: Some("current-boot".into()),
            ..key(4242, 900)
        };
        for other in [
            AdoptionKey {
                start_time: 901,
                ..current.clone()
            },
            AdoptionKey {
                boot: Some("other-boot".into()),
                ..current.clone()
            },
        ] {
            let ends = ChildEnds::new();
            let (sender, jobs) = std::sync::mpsc::channel();
            ends.arm(Handle::new(sender));
            ends.observed_session(
                &other,
                None,
                SessionObservation {
                    servers: vec!["stale".into()],
                    ..SessionObservation::default()
                },
            );
            ends.observed_session(&current, None, observation());
            ends.spawned(turn(), &current, None);
            assert_observed(
                &jobs
                    .try_recv()
                    .expect("the matching identity's observation"),
            );
            assert!(jobs.try_recv().is_err());
            assert!(ends.take_pending_observation(&current).is_empty());
            assert_eq!(
                ends.take_pending_observation(&other),
                [(
                    None,
                    SessionObservation {
                        servers: vec!["stale".into()],
                        ..SessionObservation::default()
                    }
                )]
            );
            assert!(ends.pending_observation().is_empty());
        }
    }

    #[test]
    fn an_early_rogue_report_reaches_the_supervisor_after_registration() {
        let mut harness = dudes_supervisor::testing::Harness::new();
        let (agent, _) = harness.hire_with_goal(&["/usr/bin/false"]);
        let active = harness.open_turn(agent).expect("a spawned turn");
        let child_key = harness.key_of(active).expect("the owned child");
        let ends = ChildEnds::new();
        let (sender, jobs) = std::sync::mpsc::channel();
        ends.arm(Handle::new(sender));
        ends.observed_session(&child_key, None, SessionObservation::default());
        ends.observed_session(
            &child_key,
            None,
            SessionObservation {
                servers: vec!["rogue".to_owned()],
                ..SessionObservation::default()
            },
        );
        ends.spawned(active, &child_key, None);
        for job in jobs.try_iter() {
            let Job::Input(input) = job else {
                panic!("an input");
            };
            harness.drive(*input);
        }
        let signals: Vec<_> = harness
            .records()
            .filter_map(|record| match record {
                dudes_proto::Record::BreakerBeat { signals, .. } => Some(signals.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(signals, [vec![dudes_types::BreakerSignal::IsolationLeak]]);
    }

    #[test]
    fn structured_activity_is_coalesced_and_correlated_before_queueing() {
        #[derive(Debug)]
        struct Activities;
        impl TurnReader for Activities {
            fn line(&mut self, text: &str) -> LineRead {
                LineRead {
                    activity: Some(match text {
                        "tool" => LiveActivityKind::UsingTool,
                        "compact" => LiveActivityKind::Compacting,
                        _ => LiveActivityKind::Working,
                    }),
                    ..LineRead::default()
                }
            }

            fn end(self: Box<Self>, exit: ExitStatus) -> TurnEnd {
                Box::new(Scripted::default()).end(exit)
            }
        }

        let ends = ChildEnds::new();
        let (sender, jobs) = std::sync::mpsc::channel();
        ends.arm(Handle::new(sender));
        ends.sample_activity();
        let child_key = key(4242, 900);
        let mut child = ends.watch_with(Some(&child_key), Box::new(Activities));
        child.line("tool");
        child.line("working");
        ends.flush_activities();
        assert!(
            jobs.try_recv().is_err(),
            "an unregistered child names no turn"
        );
        ends.spawned(turn(), &child_key, None);
        assert!(
            jobs.try_recv().is_err(),
            "registration leaves the kind to the next flush"
        );
        ends.flush_activities();
        assert!(matches!(
            jobs.try_recv(),
            Ok(Job::Input(input)) if matches!(input.as_ref(), Input::ActivityObserved {
                turn: observed, kind: LiveActivityKind::Working
            } if *observed == turn())
        ));
        child.line("tool");
        child.line("compact");
        assert!(jobs.try_recv().is_err());
        ends.flush_activities();
        assert!(matches!(
            jobs.try_recv(),
            Ok(Job::Input(input)) if matches!(input.as_ref(), Input::ActivityObserved {
                turn: observed, kind: LiveActivityKind::Compacting
            } if *observed == turn())
        ));
        child.line("tool");
        child.ended(EXIT_0);
        assert_end(
            &jobs
                .try_recv()
                .expect("the end replaces the unflushed kind"),
        );
        ends.flush_activities();
        assert!(jobs.try_recv().is_err());
    }

    #[test]
    fn provider_omission_reaches_output_without_changing_completed_reply() {
        let ends = ChildEnds::new();
        let (sender, jobs) = std::sync::mpsc::channel();
        ends.arm(Handle::new(sender));
        let child_key = key(4242, 900);
        let agent = dudes_types::AgentId::from_parts(1, 1);
        let mut child = ends.watch_with(
            Some(&child_key),
            Box::new(dudes_providers::pi::stream::TurnStream::new()),
        );
        child.line(&serde_json::json!({"type":"session","id":SESSION}).to_string());
        child.line(r#"{"type":"message_start","message":{"role":"assistant","content":[]}}"#);
        for index in 0..128 {
            child.line(&serde_json::json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_start","contentIndex":index}}).to_string());
        }
        assert_eq!(ends.output().usage().pending_keys, 0);
        child.line(r#"{"type":"message_update","assistantMessageEvent":{"type":"text_start","contentIndex":128}}"#);
        child.line(r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":128,"delta":"final reply"}}"#);
        assert_eq!(ends.output().usage().pending_keys, 1);
        ends.output().replace(agent, turn());
        assert!(ends.output().register(agent, turn(), &child_key));
        ends.output()
            .confirm(agent, turn(), &SessionId::new(SESSION));
        let reply = ends.output().snapshot(agent).reply.unwrap();
        assert_eq!(reply.text, "");
        assert_eq!(reply.loss, Some(crate::output::ReplyLoss::ProviderLimit));
        ends.spawned(turn(), &child_key, None);
        child.line(r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"final reply"}],"stopReason":"stop"}}"#);
        child.line(r#"{"type":"agent_settled"}"#);
        child.ended(EXIT_0);
        assert!(jobs.try_iter().any(|job| matches!(job, Job::Input(input) if matches!(input.as_ref(), Input::ChildExited { final_reply: Some(reply), outcome: dudes_proto::TurnOutcome::Completed, .. } if reply.text == "final reply" && reply.completeness == dudes_proto::ChatReplyCompleteness::Complete))));
    }

    #[test]
    fn program_activity_is_not_kept_until_sampling_starts() {
        let ends = ChildEnds::new();
        let (sender, jobs) = std::sync::mpsc::channel();
        ends.arm(Handle::new(sender));
        let child_key = key(4242, 900);
        ends.spawned(turn(), &child_key, None);
        ends.observed_activity(&child_key, LiveActivityKind::UsingTool);
        ends.flush_activities();
        assert!(jobs.try_recv().is_err());
        assert!(
            ends.pending_activity
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
        );
    }

    #[test]
    fn pending_structured_activity_has_a_fixed_key_cap() {
        let ends = ChildEnds::new();
        ends.sample_activity();
        for pid in 1..=129 {
            ends.observed_activity(&key(pid, 900), LiveActivityKind::Working);
        }
        let pending = ends
            .pending_activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert_eq!(pending.len(), 128);
        assert!(!pending.contains_key(&(1, 900, None)));
        assert_eq!(
            pending.get(&(129, 900, None)),
            Some(&LiveActivityKind::Working)
        );
    }

    #[test]
    fn a_child_watched_with_no_reader_armed_still_ends() {
        let ends = ChildEnds::new();
        let (sender, jobs) = std::sync::mpsc::channel();
        ends.arm(Handle::new(sender));
        let (first, second) = (key(4242, 900), key(4243, 901));
        ends.spawned(turn(), &first, None);
        ends.spawned(turn(), &second, None);
        ends.read_next_with(Some(Box::new(Scripted::default())));
        let mut read = ends.watch(Some(&first));
        let mut unread = ends.watch(Some(&second));

        unread.line(OPEN);
        assert!(
            jobs.try_recv().is_err(),
            "the armed reader went to the first child alone"
        );
        unread.ended(EXIT_0);
        assert!(matches!(
            jobs.try_recv(),
            Ok(Job::Input(input)) if matches!(
                input.as_ref(),
                Input::ChildExited {
                    turn: ended,
                    usage: None,
                    outcome: dudes_proto::TurnOutcome::NoResult,
                    last_message: None,
                    ..
                } if *ended == turn()
            )
        ));
        assert!(
            !ends.open().contains_key(&second.pid),
            "the unread child's registration was released"
        );
        read.line(OPEN);
        assert_observed(&jobs.try_recv().expect("control: the armed child observes"));
    }

    #[test]
    fn a_refused_attribution_warns_and_none_does_not() {
        assert_eq!(attribution_warnings(1), 1);
        assert_eq!(
            attribution_warnings(0),
            0,
            "control: a matching result must not warn"
        );
    }

    /// The register answers for the child that was registered, once — and a second end for the
    /// same key finds nothing, which is what keeps a watcher that somehow reported twice from
    /// ending two turns.
    #[test]
    fn a_registered_child_is_claimed_exactly_once() {
        let ends = ChildEnds::new();
        ends.spawned(turn(), &key(4242, 900), None);
        assert_eq!(ends.claim(&key(4242, 900)), Some(turn()));
        assert_eq!(ends.claim(&key(4242, 900)), None);
    }

    /// A recycled identifier is refused: same pid, different start value, so it is a different
    /// process and not this turn's child. The control on the line above is that the *matching* key
    /// is claimed — without it, a `claim` that always answered `None` would pass this test.
    #[test]
    fn a_recycled_identifier_does_not_answer_for_the_turn() {
        let ends = ChildEnds::new();
        ends.spawned(turn(), &key(4242, 900), None);
        assert_eq!(ends.claim(&key(4242, 901)), None, "a different start value");
        assert_eq!(ends.claim(&key(4243, 900)), None, "a different identifier");
        assert_eq!(
            ends.claim(&key(4242, 900)),
            Some(turn()),
            "control: the key that was registered still answers"
        );
    }

    /// A boot identity that disagrees with the recorded one names a different recording, even at
    /// the same identifier and start value — the axis a boot-relative start value collides on.
    #[test]
    fn a_boot_identity_that_differs_does_not_answer() {
        let ends = ChildEnds::new();
        let recorded = AdoptionKey {
            boot: Some("boot-a".to_owned()),
            ..key(4242, 900)
        };
        ends.spawned(turn(), &recorded, None);
        assert_eq!(
            ends.claim(&AdoptionKey {
                boot: Some("boot-b".to_owned()),
                ..key(4242, 900)
            }),
            None
        );
        assert_eq!(ends.claim(&key(4242, 900)), None, "and neither does none");
        assert_eq!(ends.claim(&recorded), Some(turn()), "control");
    }

    /// A child whose key could not be read drops its stream rather than guessing a turn, and does
    /// so without panicking or reaching a loop it has no handle on.
    #[test]
    fn a_child_with_no_key_ends_quietly() {
        let ends = ChildEnds::new();
        ends.read_next_with(Some(Box::new(Scripted::default())));
        let mut child = ends.watch(None);
        child.line(OPEN);
        child.ended(EXIT_0);
        assert!(ends.open().is_empty());
    }
    #[derive(Debug, Default)]
    struct QueuedInput(Arc<Mutex<Vec<ProtocolAction>>>);
    impl ChildInput for QueuedInput {
        fn send(&self, action: ProtocolAction) -> std::io::Result<()> {
            self.0.lock().unwrap().push(action);
            Ok(())
        }
    }
    fn protocol_spec() -> LaunchSpec {
        LaunchSpec {
            argv: vec!["codex".into()],
            env: vec![],
            cwd: "/repo".into(),
            stdin: LaunchInput::Protocol(CodexStartup::new(
                "/profile".into(),
                "/agent".into(),
                "/repo".into(),
                "task".into(),
                dudes_providers::adapter::TurnSession::Assigned,
                dudes_types::ModelSelection::provider_default(),
            )),
        }
    }
    #[test]
    fn protocol_failure_is_exact_idempotent_and_overrides_an_early_successful_exit() {
        for register_first in [true, false] {
            let ends = ChildEnds::new();
            let (sender, jobs) = std::sync::mpsc::channel();
            ends.arm(Handle::new(sender));
            let owned = key(4242, 900);
            let other = key(4242, 901);
            if register_first {
                ends.spawned(turn(), &owned, None);
            }
            ends.read_next_for(Some(Box::new(Scripted::default())), &protocol_spec(), None);
            let child = ends.watch_protocol(&owned, Box::new(QueuedInput::default()));
            ends.protocol_failed(&other);
            assert!(jobs.try_recv().is_err());
            ends.protocol_failed(&owned);
            ends.protocol_failed(&owned);
            assert!(
                matches!(jobs.try_recv().unwrap(),Job::ProtocolFailed(key) if protocol_key(&key)==protocol_key(&owned))
            );
            assert!(jobs.try_recv().is_err());
            child.ended(EXIT_0);
            if !register_first {
                assert!(jobs.try_recv().is_err());
                ends.spawned(turn(), &owned, None);
            }
            assert!(
                matches!(jobs.try_recv().unwrap(),Job::Input(input) if matches!(input.as_ref(),Input::ChildExited { outcome:dudes_proto::TurnOutcome::Failed { .. },final_reply:None,last_message:None,.. }))
            );
            ends.protocol_failed(&owned);
            assert!(jobs.try_recv().is_err());
            assert!(ends.protocols.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn protocol_bookkeeping_is_bounded_and_overflow_admits_no_writes() {
        let ends = ChildEnds::new();
        let (sender, jobs) = std::sync::mpsc::channel();
        ends.arm(Handle::new(sender));
        let mut children = Vec::new();
        for index in 0..MAX_PROTOCOL_CHILDREN {
            ends.read_next_for(Some(Box::new(Scripted::default())), &protocol_spec(), None);
            children.push(ends.watch_protocol(
                &key(u32::try_from(index).unwrap() + 1, 900),
                Box::new(QueuedInput::default()),
            ));
        }
        let queued = QueuedInput::default();
        let captured = Arc::clone(&queued.0);
        ends.read_next_for(Some(Box::new(Scripted::default())), &protocol_spec(), None);
        let excess = ends.watch_protocol(&key(9999, 900), Box::new(queued));
        assert_eq!(ends.protocols.lock().unwrap().len(), MAX_PROTOCOL_CHILDREN);
        assert_eq!(*captured.lock().unwrap(), [ProtocolAction::Fail]);
        assert!(matches!(jobs.try_recv().unwrap(), Job::ProtocolFailed(_)));
        excess.ended(EXIT_0);
        for child in children {
            child.ended(EXIT_0);
        }
        assert!(ends.protocols.lock().unwrap().is_empty());
    }
    #[test]
    fn a_missing_profile_prevents_the_task_frame_after_handshake() {
        let ends = ChildEnds::new();
        let (sender, jobs) = std::sync::mpsc::channel();
        ends.arm(Handle::new(sender));
        let spec = protocol_spec();
        let LaunchInput::Protocol(startup) = &spec.stdin else {
            panic!("protocol")
        };
        ends.read_next_for(
            Some(Box::new(
                dudes_providers::codex::app_server::AppServerReader::new(startup.clone()),
            )),
            &spec,
            None,
        );
        let queued = QueuedInput::default();
        let captured = Arc::clone(&queued.0);
        let owned = key(4242, 900);
        let mut child = ends.watch_protocol(&owned, Box::new(queued));
        child.line(
            r#"{"id":1,"result":{"userAgent":"dudes/0.160.0 (test)","codexHome":"/profile"}}"#,
        );
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let roots = serde_json::json!(["/agent/tmp", "/agent/outbox"]);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let roots = serde_json::json!(["/agent/tmp"]);
        child.line(&serde_json::json!({"id":2,"result":{"thread":{"id":"01a0e1d9-3860-7800-a9ff-2994ef3b1c6f","status":{"type":"idle"}},"cwd":"/repo","approvalPolicy":"never","sandbox":{"type":"workspaceWrite","writableRoots":roots,"networkAccess":false,"excludeTmpdirEnvVar":true,"excludeSlashTmp":true}}}).to_string());
        assert!(
            captured
                .lock()
                .unwrap()
                .iter()
                .all(|action| !matches!(action,ProtocolAction::Write(frame) if frame.is_task()))
        );
        assert!(matches!(
            captured.lock().unwrap().last(),
            Some(ProtocolAction::Fail)
        ));
        assert!(
            matches!(jobs.try_recv().unwrap(),Job::ProtocolFailed(key) if protocol_key(&key)==protocol_key(&owned))
        );
    }

    #[derive(Debug)]
    struct HostInput {
        ends: Arc<ChildEnds>,
        key: AdoptionKey,
    }
    impl ChildInput for HostInput {
        fn send(&self, action: ProtocolAction) -> std::io::Result<()> {
            if action == ProtocolAction::Fail {
                self.ends.protocol_failed(&self.key);
            }
            Ok(())
        }
    }
    #[test]
    fn the_readers_refusal_is_journaled_when_the_host_also_reports_the_failure() {
        let ends = ChildEnds::new();
        let (sender, jobs) = std::sync::mpsc::channel();
        ends.arm(Handle::new(sender));
        let spec = protocol_spec();
        let LaunchInput::Protocol(startup) = &spec.stdin else {
            panic!("protocol")
        };
        ends.read_next_for(
            Some(Box::new(
                dudes_providers::codex::app_server::AppServerReader::new(startup.clone()),
            )),
            &spec,
            None,
        );
        let owned = key(4242, 900);
        ends.spawned(turn(), &owned, None);
        let mut child = ends.watch_protocol(
            &owned,
            Box::new(HostInput {
                ends: Arc::clone(&ends),
                key: owned.clone(),
            }),
        );
        child.line(
            r#"{"id":1,"result":{"userAgent":"dudes/0.161.0 (test)","codexHome":"/profile"}}"#,
        );
        assert!(matches!(jobs.try_recv().unwrap(), Job::ProtocolFailed(_)));
        child.ended(EXIT_0);
        let Job::Input(input) = jobs.try_recv().unwrap() else {
            panic!("the turn's end")
        };
        let Input::ChildExited { outcome, .. } = *input else {
            panic!("a child exit")
        };
        assert_eq!(
            outcome,
            dudes_proto::TurnOutcome::Failed {
                reason: "Codex streaming requires codex-cli 0.160.x, but the installed Codex reports dudes/0.161.0".into()
            }
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_stopped_journal_prevents_the_task_frame_after_a_valid_handshake() {
        struct ProfileProbe;
        impl crate::predicate::PredicateRunner for ProfileProbe {
            fn run(
                &self,
                spec: &LaunchSpec,
                _: std::time::Duration,
            ) -> std::io::Result<crate::predicate::PredicateOutcome> {
                let output = if spec.argv[0] == "/usr/bin/osascript" {
                    r#"{"synchronized":true,"config":false,"requirements":false}"#
                } else {
                    "codex-cli 0.160.0\n"
                };
                Ok(crate::predicate::PredicateOutcome::Finished {
                    exit: Some(0),
                    output_tail: output.into(),
                    timed_out: false,
                })
            }
            fn sweep(&self) {}
        }
        for stopped in 0..5 {
            let root = tempfile::tempdir().unwrap();
            let work = root.path().join("work");
            std::fs::create_dir(&work).unwrap();
            let state = root.path().join("state");
            let agent = dudes_types::AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
            let outbox = crate::mailbox::outbox_fixture(&state, agent);
            let profile =
                crate::codex_profile::CodexProfile::new(&state, &[], Arc::new(ProfileProbe));
            let startup = CodexStartup::new(
                state.join("providers/codex"),
                state.join("agents").join(agent.to_string()),
                work.clone(),
                "task".into(),
                dudes_providers::adapter::TurnSession::Assigned,
                dudes_types::ModelSelection::provider_default(),
            );
            let mut spec = LaunchSpec {
                argv: vec!["/fake/codex".into()],
                env: vec![],
                cwd: work,
                stdin: LaunchInput::Null,
            };
            let prepared = profile.prepare(&mut spec, &startup).unwrap();
            assert!(profile.before_task(&prepared).is_ok());
            spec.stdin = LaunchInput::Protocol(prepared.clone());
            let ends = ChildEnds::new();
            let (sender, jobs) = std::sync::mpsc::channel();
            let handle = Handle::new(sender);
            ends.arm(handle.clone());
            ends.configure_codex_profile(profile);
            *ends.next_outbox.lock().unwrap() = Some(outbox);
            ends.read_next_for(
                Some(Box::new(
                    dudes_providers::codex::app_server::AppServerReader::new(prepared.clone()),
                )),
                &spec,
                None,
            );
            let queued = QueuedInput::default();
            let captured = Arc::clone(&queued.0);
            let owned = key(4242, 900);
            let mut child = ends.watch_protocol(&owned, Box::new(queued));
            child.line(&serde_json::json!({"id":1,"result":{"userAgent":"dudes/0.160.0 (test)","codexHome":prepared.home()}}).to_string());
            if stopped == 1 {
                handle.fence_journal();
            } else if stopped == 2 {
                handle.publish_journal_stopped(dudes_types::Lsn::ZERO);
            } else if stopped >= 3 {
                let outbox = prepared.agent_dir().join("outbox");
                std::fs::rename(&outbox, root.path().join("retained-outbox")).unwrap();
                if stopped == 3 {
                    use std::os::unix::fs::DirBuilderExt as _;
                    std::fs::DirBuilder::new()
                        .mode(0o700)
                        .create(&outbox)
                        .unwrap();
                }
            }
            child.line(&serde_json::json!({"id":2,"result":{"thread":{"id":"01a0e1d9-3860-7800-a9ff-2994ef3b1c6f","status":{"type":"idle"}},"cwd":prepared.cwd(),"approvalPolicy":"never","sandbox":{"type":"workspaceWrite","writableRoots":[prepared.agent_dir().join("tmp"),prepared.agent_dir().join("outbox")],"networkAccess":false,"excludeTmpdirEnvVar":true,"excludeSlashTmp":true}}}).to_string());
            let actions = captured.lock().unwrap();
            assert_eq!(
                actions.iter().any(
                    |action| matches!(action, ProtocolAction::Write(frame) if frame.is_task())
                ),
                stopped == 0
            );
            if stopped != 0 {
                assert!(matches!(actions.last(), Some(ProtocolAction::Fail)));
                assert!(
                    matches!(jobs.try_recv().unwrap(), Job::ProtocolFailed(key) if protocol_key(&key) == protocol_key(&owned))
                );
            }
        }
    }
}
