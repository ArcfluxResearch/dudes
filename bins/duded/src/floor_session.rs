//! Run-local authentication for the read-only web Floor.
//!
//! The local RPC route and the loopback HTTP listener share this module. Raw bootstrap, cookie and
//! CSRF values leave only at their one required response; the retained state contains SHA-256
//! digests. Stopping the owning listener invalidates and clears the complete state before the run
//! releases its transport.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use dudes_proto::record::Hex32;
use dudes_proto::rpc::floor::{
    ExportedSession, SESSION_EXPIRES_IN_S, SessionExport, SessionIssueResult,
};
use dudes_supervisor::Clock;
use dudes_types::Timestamp;
use sha2::{Digest as _, Sha256};

use crate::baseline::CheckRuns;
use crate::floor_ticket::TicketLimits;

/// Cookie name used by the read-only Floor session.
pub const SESSION_COOKIE_NAME: &str = "dudes_floor_session";
/// Header carrying the in-memory CSRF nonce on authenticated dynamic requests.
pub const CSRF_HEADER_NAME: &str = "X-Dudes-CSRF";

const BOOTSTRAP_EXPIRES_MS: i64 = 60_000;
const IDLE_EXPIRES_MS: i64 = 30 * 60_000;
const ABSOLUTE_EXPIRES_MS: i64 = 8 * 60 * 60_000;
const RECOVERY_WINDOW_MS: i64 = 2_000;
const MAX_PENDING_BOOTSTRAPS: usize = 4;
const MAX_LIVE_SESSIONS: usize = 4;
const MAX_WEBSOCKETS_PER_SESSION: usize = 2;

/// The issue route cannot mint another bootstrap while its bounded run-local table is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueError {
    /// Four still-live bootstraps already await exchange.
    Capacity,
    /// The listener that owned this run-local state has stopped.
    RunEnded,
}

impl fmt::Display for IssueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capacity => f.write_str("the Floor bootstrap table is full"),
            Self::RunEnded => f.write_str("the Floor session run has ended"),
        }
    }
}

/// The one response that crosses from the session module to the HTTP writer.
pub(crate) struct Exchange {
    pub(crate) cookie: String,
    pub(crate) csrf: String,
}

impl fmt::Debug for Exchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Exchange")
            .field("cookie", &"<redacted>")
            .field("csrf", &"<redacted>")
            .finish()
    }
}

/// The one fresh secret returned by a successful recovery.
pub(crate) struct Recovery {
    pub(crate) csrf: String,
}

impl fmt::Debug for Recovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Recovery")
            .field("csrf", &"<redacted>")
            .finish()
    }
}

/// An unforgeable admission to the future read-only projection facade.
pub struct ReadOnlySession {
    _private: (),
}

impl fmt::Debug for ReadOnlySession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReadOnlySession(<redacted>)")
    }
}

/// The read-only half of one daemon run's Floor session state.
#[derive(Clone)]
pub struct FloorSessionAuthorizer(Arc<Inner>);

impl fmt::Debug for FloorSessionAuthorizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FloorSessionAuthorizer")
            .field("origin", &self.0.origin)
            .finish_non_exhaustive()
    }
}

impl FloorSessionAuthorizer {
    /// Revoke one presented session under the registry lock; test-only caller access.
    #[doc(hidden)]
    pub fn revoke_for_test(&self, cookie: &str) -> bool {
        let cookie = digest(cookie.as_bytes());
        let mut state = self.0.registry.lock();
        state.sessions.remove(&cookie).is_some_and(|session| {
            session.revoked.store(true, Ordering::Release);
            true
        })
    }

    /// Run `hook` once, as the next reservation asks for the registry lock; test-only caller access.
    #[doc(hidden)]
    pub fn before_next_reserve_for_test(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .0
            .reserve_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }

    /// Admit one exact-origin read and advance only its idle deadline.
    #[must_use]
    pub fn authorize_read(
        &self,
        host: &str,
        origin: &str,
        cookie: &str,
        csrf: &str,
    ) -> Option<ReadOnlySession> {
        if !self.0.authority_matches(host, origin) {
            return None;
        }
        if !self.0.valid.load(Ordering::Acquire) {
            return None;
        }
        let cookie = digest(cookie.as_bytes());
        let csrf = digest(csrf.as_bytes());
        let registry::Locked { mut state, now } = self.0.registry.lock_at_now();
        if !self.0.valid.load(Ordering::Acquire) {
            return None;
        }
        state.prune(now);
        let session = state.sessions.get_mut(&cookie)?;
        if session.csrf != Some(csrf) {
            return None;
        }
        session.last_used = now;
        Some(ReadOnlySession { _private: () })
    }

    /// Authenticate one browser upgrade and atomically acquire its per-session socket slot.
    #[must_use]
    pub(crate) fn authorize_stream(
        &self,
        host: &str,
        origin: &str,
        cookie: &str,
        csrf: &str,
    ) -> Option<StreamSession> {
        let ((), cookie, revoked) = self.admit_socket(host, origin, cookie, csrf, |session| {
            (session.websockets < MAX_WEBSOCKETS_PER_SESSION).then(|| session.websockets += 1)
        })?;
        Some(StreamSession {
            inner: Arc::clone(&self.0),
            cookie,
            revoked,
        })
    }

    #[must_use]
    pub(crate) fn authorize_terminal(
        &self,
        host: &str,
        origin: &str,
        cookie: &str,
        csrf: &str,
    ) -> Option<TerminalSession> {
        let (browser_epoch, cookie, revoked) =
            self.admit_socket(host, origin, cookie, csrf, |session| {
                if session.terminal.is_some() {
                    return None;
                }
                let browser_epoch = self.0.last_browser_epoch.fetch_add(1, Ordering::AcqRel) + 1;
                session.terminal = Some(TerminalSlot {
                    browser_epoch,
                    grant: None,
                });
                Some(browser_epoch)
            })?;
        Some(TerminalSession {
            inner: Arc::clone(&self.0),
            cookie,
            revoked,
            browser_epoch,
        })
    }

    /// Authenticate one upgrade and let `take` claim its session's slot under the registry lock.
    /// Only a claimed slot advances the session's idle time.
    fn admit_socket<T>(
        &self,
        host: &str,
        origin: &str,
        cookie: &str,
        csrf: &str,
        take: impl FnOnce(&mut Session) -> Option<T>,
    ) -> Option<(T, [u8; 32], Arc<AtomicBool>)> {
        if !self.0.authority_matches(host, origin) || !self.0.valid.load(Ordering::Acquire) {
            return None;
        }
        let cookie = digest(cookie.as_bytes());
        let csrf = digest(csrf.as_bytes());
        let registry::Locked { mut state, now } = self.0.registry.lock_at_now();
        if !self.0.valid.load(Ordering::Acquire) {
            return None;
        }
        state.prune(now);
        let session = state.sessions.get_mut(&cookie)?;
        if session.csrf != Some(csrf) {
            return None;
        }
        let taken = take(session)?;
        session.last_used = now;
        session.recovery_pending_until = None;
        Some((taken, cookie, Arc::clone(&session.revoked)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChannelKey {
    pub(crate) session_digest: Hex32,
    pub(crate) browser_epoch: u64,
}

/// Whether an admitted terminal message counts as session activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdleClock {
    Advance,
    Hold,
}

struct TerminalSlot {
    browser_epoch: u64,
    grant: Option<SlotGrant>,
}

struct SlotGrant {
    handle_digest: [u8; 32],
    /// Shared with the daemon loop's record of the attachment, which sets it once the
    /// attachment stops being live.
    attachment_revoked: Arc<AtomicBool>,
}

pub(crate) struct TerminalSession {
    inner: Arc<Inner>,
    cookie: [u8; 32],
    revoked: Arc<AtomicBool>,
    browser_epoch: u64,
}

impl TerminalSession {
    pub(crate) fn key(&self) -> ChannelKey {
        ChannelKey {
            session_digest: Hex32::new(self.cookie),
            browser_epoch: self.browser_epoch,
        }
    }

    fn with_slot<T>(
        &self,
        idle: IdleClock,
        check: impl FnOnce(&mut TerminalSlot) -> Option<T>,
    ) -> Option<T> {
        if !self.inner.valid.load(Ordering::Acquire) || self.revoked.load(Ordering::Acquire) {
            return None;
        }
        let registry::Locked { mut state, now } = self.inner.registry.lock_at_now();
        if !self.inner.valid.load(Ordering::Acquire) {
            return None;
        }
        state.prune(now);
        let session = state.sessions.get_mut(&self.cookie)?;
        let slot = session
            .terminal
            .as_mut()
            .filter(|slot| slot.browser_epoch == self.browser_epoch)?;
        let answer = check(slot)?;
        if idle == IdleClock::Advance {
            session.last_used = now;
        }
        Some(answer)
    }

    pub(crate) fn is_valid(&self) -> bool {
        self.with_slot(IdleClock::Hold, |_| Some(())).is_some()
    }

    pub(crate) fn touch(&self) -> bool {
        self.with_slot(IdleClock::Advance, |_| Some(())).is_some()
    }

    /// Keep only the new handle's digest, and return the handle for its one delivery.
    pub(crate) fn mint_grant(&self, attachment_revoked: Arc<AtomicBool>) -> Option<String> {
        self.with_slot(IdleClock::Hold, |slot| {
            let handle = mint_secret();
            slot.grant = Some(SlotGrant {
                handle_digest: digest(handle.as_bytes()),
                attachment_revoked,
            });
            Some(handle)
        })
    }

    pub(crate) fn admit(&self, handle: &str, idle: IdleClock) -> bool {
        let presented = digest(handle.as_bytes());
        self.with_slot(idle, |slot| {
            slot.grant
                .as_ref()
                .filter(|grant| {
                    grant.handle_digest == presented
                        && !grant.attachment_revoked.load(Ordering::Acquire)
                })
                .map(drop)
        })
        .is_some()
    }

    pub(crate) fn grant_live(&self) -> bool {
        self.with_slot(IdleClock::Hold, |slot| {
            slot.grant
                .as_ref()
                .filter(|grant| !grant.attachment_revoked.load(Ordering::Acquire))
                .map(drop)
        })
        .is_some()
    }

    pub(crate) fn release_grant(&self) {
        if let Some(slot) = self
            .inner
            .registry
            .lock()
            .sessions
            .get_mut(&self.cookie)
            .and_then(|session| session.terminal.as_mut())
            .filter(|slot| slot.browser_epoch == self.browser_epoch)
        {
            slot.grant = None;
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let mut state = self.inner.registry.lock();
        if let Some(session) = state.sessions.get_mut(&self.cookie)
            && session
                .terminal
                .as_ref()
                .is_some_and(|slot| slot.browser_epoch == self.browser_epoch)
        {
            session.terminal = None;
        }
    }
}

/// One authenticated, run-local WebSocket slot. Its identity never crosses the browser wire.
pub(crate) struct StreamSession {
    inner: Arc<Inner>,
    cookie: [u8; 32],
    revoked: Arc<AtomicBool>,
}

/// Internal session identity for writer-ordered Floor reads and mutation permits.
#[derive(Clone)]
pub struct SessionIdentity {
    inner: Arc<Inner>,
    cookie: [u8; 32],
    revoked: Arc<AtomicBool>,
}

impl SessionIdentity {
    /// A stable key for this session, for a limit that is per session. It is a digest of the
    /// session cookie, so it names the session and reveals nothing the cookie does not.
    pub(crate) const fn limit_key(&self) -> [u8; 32] {
        self.cookie
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PermitFailure {
    Expired,
    Revoked,
}

impl fmt::Display for PermitFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expired => f.write_str("Floor session expired before append"),
            Self::Revoked => f.write_str("Floor session revoked before append"),
        }
    }
}

impl std::error::Error for PermitFailure {}

/// The registry lock grants one append authority only until its fsync returns.
pub(crate) struct MutationPermit<'a> {
    _guard: MutexGuard<'a, State>,
}

/// Which browser action a request identity belongs to. Each action's durable index is its own
/// namespace, so its in-flight guard is too: an identity in flight for one action never answers
/// for another. Pause and resume are one action with one index, so an identity in flight on one
/// route meets the other as a conflict. A goal change and a renewal share one durable
/// index that records each key's kind, and each has its own guard here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ActionKind {
    Onboard,
    Answer,
    Control,
    Goal,
    Renew,
    Rename,
    Look,
    Retire,
    Chat,
}

pub(crate) enum ReserveResult {
    Owner(AttemptReservation),
    Pending,
    Conflict,
    Forbidden,
}

/// A run-local in-flight guard; it is never retry truth after a restart.
pub(crate) struct AttemptReservation {
    inner: Arc<Inner>,
    attempt: (ActionKind, Hex32),
    retained: bool,
}

impl AttemptReservation {
    pub(crate) fn retain_pending(&mut self) {
        self.retained = true;
    }
}

impl Drop for AttemptReservation {
    fn drop(&mut self) {
        if !self.retained {
            self.inner.registry.lock().inflight.remove(&self.attempt);
        }
    }
}

impl fmt::Debug for SessionIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionIdentity(<redacted>)")
    }
}

impl SessionIdentity {
    pub(crate) fn reserve(&self, kind: ActionKind, attempt: Hex32, digest: Hex32) -> ReserveResult {
        // The test hook and the lock are one statement: the hook runs before the lock, and the
        // clock is read only after it.
        let registry::Locked { mut state, now } = {
            let hook = self
                .inner
                .reserve_hook
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(hook) = hook {
                hook();
            }
            self.inner.registry.lock_at_now()
        };
        if !self.inner.valid.load(Ordering::Acquire) || self.revoked.load(Ordering::Acquire) {
            return ReserveResult::Forbidden;
        }
        state.prune(now);
        if !state.sessions.contains_key(&self.cookie) {
            return ReserveResult::Forbidden;
        }
        let attempt = (kind, attempt);
        if let Some(existing) = state.inflight.get(&attempt) {
            return if *existing == digest {
                ReserveResult::Pending
            } else {
                ReserveResult::Conflict
            };
        }
        state.inflight.insert(attempt, digest);
        ReserveResult::Owner(AttemptReservation {
            inner: Arc::clone(&self.inner),
            attempt,
            retained: false,
        })
    }
    pub(crate) fn digest(&self) -> Hex32 {
        Hex32::new(self.cookie)
    }

    pub(crate) fn is_valid(&self) -> bool {
        if !self.inner.valid.load(Ordering::Acquire) || self.revoked.load(Ordering::Acquire) {
            return false;
        }
        let registry::Locked { mut state, now } = self.inner.registry.lock_at_now();
        state.prune(now);
        self.inner.valid.load(Ordering::Acquire) && state.sessions.contains_key(&self.cookie)
    }

    pub(crate) fn permit(&self) -> Result<MutationPermit<'_>, PermitFailure> {
        let registry::Locked { mut state, now } = self.inner.registry.lock_at_now();
        if !self.inner.valid.load(Ordering::Acquire) || self.revoked.load(Ordering::Acquire) {
            return Err(PermitFailure::Revoked);
        }
        state.prune(now);
        if !state.sessions.contains_key(&self.cookie) {
            return Err(PermitFailure::Expired);
        }
        Ok(MutationPermit { _guard: state })
    }
}

impl StreamSession {
    pub(crate) fn identity(&self) -> SessionIdentity {
        SessionIdentity {
            inner: Arc::clone(&self.inner),
            cookie: self.cookie,
            revoked: Arc::clone(&self.revoked),
        }
    }
    /// Revalidate without refreshing idle time.
    #[must_use]
    pub(crate) fn is_valid(&self) -> bool {
        if !self.inner.valid.load(Ordering::Acquire) {
            return false;
        }
        let registry::Locked { mut state, now } = self.inner.registry.lock_at_now();
        state.prune(now);
        self.inner.valid.load(Ordering::Acquire) && state.sessions.contains_key(&self.cookie)
    }

    /// A successfully decoded client `floor.subscribe` is admitted activity.
    pub(crate) fn touch_subscribe(&self) -> bool {
        if !self.inner.valid.load(Ordering::Acquire) {
            return false;
        }
        let registry::Locked { mut state, now } = self.inner.registry.lock_at_now();
        state.prune(now);
        if !self.inner.valid.load(Ordering::Acquire) {
            return false;
        }
        let Some(session) = state.sessions.get_mut(&self.cookie) else {
            return false;
        };
        session.last_used = now;
        true
    }
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        let mut state = self.inner.registry.lock();
        if let Some(session) = state.sessions.get_mut(&self.cookie) {
            session.websockets = session.websockets.saturating_sub(1);
        }
    }
}

/// Minting, exchange and authorization state for exactly one daemon run.
#[derive(Clone)]
pub struct FloorSessions(Arc<Inner>);

impl fmt::Debug for FloorSessions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FloorSessions")
            .field("origin", &self.0.origin)
            .finish_non_exhaustive()
    }
}

type ReserveHook = Box<dyn FnOnce() + Send>;

struct Inner {
    host: String,
    origin: String,
    valid: AtomicBool,
    last_browser_epoch: AtomicU64,
    registry: registry::Registry,
    reserve_hook: Mutex<Option<ReserveHook>>,
}

/// The session registry: its state, and the clock that dates it, reachable only together.
///
/// A private module, so the clock is a field no method outside it can name. Every session-validity
/// decision reads the time through [`registry::Registry::lock_at_now`], which takes the registry
/// lock first, so a clock read planted above a lock does not compile.
mod registry {
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    use dudes_supervisor::Clock;
    use dudes_types::Timestamp;

    use super::State;

    pub(super) struct Registry {
        clock: Arc<dyn Clock + Send + Sync>,
        state: Mutex<State>,
    }

    /// The locked registry state, and the time read after the lock was taken.
    pub(super) struct Locked<'a> {
        pub(super) state: MutexGuard<'a, State>,
        pub(super) now: Timestamp,
    }

    impl Registry {
        pub(super) fn new(clock: Arc<dyn Clock + Send + Sync>) -> Self {
            Self {
                clock,
                state: Mutex::new(State::default()),
            }
        }

        /// Take the lock, then read the clock: the only way this registry's clock is read.
        pub(super) fn lock_at_now(&self) -> Locked<'_> {
            let state = self.lock();
            let now = self.clock.now();
            Locked { state, now }
        }

        /// Take the lock and read no time, for a path that decides nothing by it.
        pub(super) fn lock(&self) -> MutexGuard<'_, State> {
            self.state.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }
}

#[derive(Default)]
struct State {
    imported: bool,
    pending: BTreeMap<[u8; 32], Timestamp>,
    sessions: BTreeMap<[u8; 32], Session>,
    inflight: BTreeMap<(ActionKind, Hex32), Hex32>,
}

struct Session {
    revoked: Arc<AtomicBool>,
    /// `None` for a session imported from the previous run, which matches no secret until the
    /// recovery route issues one.
    csrf: Option<[u8; 32]>,
    created: Timestamp,
    last_used: Timestamp,
    websockets: usize,
    terminal: Option<TerminalSlot>,
    recovery_pending_until: Option<Timestamp>,
}

impl Session {
    fn live_at(&self, now: Timestamp) -> bool {
        now.millis_since(self.created) < ABSOLUTE_EXPIRES_MS
            && now.millis_since(self.last_used) < IDLE_EXPIRES_MS
    }
}

impl State {
    fn prune(&mut self, now: Timestamp) {
        self.pending
            .retain(|_, issued| now.millis_since(*issued) < BOOTSTRAP_EXPIRES_MS);
        self.sessions.retain(|_, session| session.live_at(now));
    }
}

impl FloorSessions {
    /// Admit one exact-origin browser action — onboarding, an answer, a pause or a resume — and
    /// retain only its session digest.
    pub(crate) fn authorize_onboard(
        &self,
        host: &str,
        origin: &str,
        cookie: &str,
        csrf: &str,
    ) -> Option<SessionIdentity> {
        if !self.0.authority_matches(host, origin) {
            return None;
        }
        let cookie = digest(cookie.as_bytes());
        let csrf = digest(csrf.as_bytes());
        let registry::Locked { mut state, now } = self.0.registry.lock_at_now();
        if !self.0.valid.load(Ordering::Acquire) {
            return None;
        }
        state.prune(now);
        let session = state.sessions.get_mut(&cookie)?;
        if session.csrf != Some(csrf) {
            return None;
        }
        session.last_used = now;
        Some(SessionIdentity {
            inner: Arc::clone(&self.0),
            cookie,
            revoked: Arc::clone(&session.revoked),
        })
    }
    /// Create empty session state for the listener's exact run-local origin.
    #[must_use]
    pub fn new<C>(addr: SocketAddr, clock: Arc<C>) -> Self
    where
        C: Clock + Send + Sync + 'static,
    {
        debug_assert!(addr.ip().is_loopback());
        let host = addr.to_string();
        Self(Arc::new(Inner {
            origin: format!("http://{host}"),
            host,
            valid: AtomicBool::new(true),
            last_browser_epoch: AtomicU64::new(0),
            registry: registry::Registry::new(clock),
            reserve_hook: Mutex::new(None),
        }))
    }

    /// A narrower clone that can authenticate reads but cannot mint or exchange credentials.
    #[must_use]
    pub fn authorizer(&self) -> FloorSessionAuthorizer {
        FloorSessionAuthorizer(Arc::clone(&self.0))
    }

    /// Whether Host and Origin exactly name this listener.
    #[must_use]
    pub(crate) fn authority_matches(&self, host: &str, origin: &str) -> bool {
        self.0.authority_matches(host, origin)
    }

    /// Mint one 60-second bootstrap while retaining only its digest.
    pub fn issue(&self) -> Result<SessionIssueResult, IssueError> {
        if !self.0.valid.load(Ordering::Acquire) {
            return Err(IssueError::RunEnded);
        }
        let registry::Locked { mut state, now } = self.0.registry.lock_at_now();
        if !self.0.valid.load(Ordering::Acquire) {
            return Err(IssueError::RunEnded);
        }
        state.prune(now);
        if state.pending.len() >= MAX_PENDING_BOOTSTRAPS {
            return Err(IssueError::Capacity);
        }
        let bootstrap = loop {
            let candidate = mint_secret();
            if !state.pending.contains_key(&digest(candidate.as_bytes())) {
                break candidate;
            }
        };
        state.pending.insert(digest(bootstrap.as_bytes()), now);
        Ok(SessionIssueResult {
            base_url: self.0.origin.clone(),
            bootstrap,
            expires_in_s: SESSION_EXPIRES_IN_S,
        })
    }

    /// Exchange one valid bearer under this listener's exact Host and Origin.
    #[must_use]
    pub(crate) fn exchange(&self, host: &str, origin: &str, bootstrap: &[u8]) -> Option<Exchange> {
        if !self.0.authority_matches(host, origin) {
            return None;
        }
        if !self.0.valid.load(Ordering::Acquire) {
            return None;
        }
        let bootstrap = digest(bootstrap);
        let registry::Locked { mut state, now } = self.0.registry.lock_at_now();
        if !self.0.valid.load(Ordering::Acquire) {
            return None;
        }
        state.prune(now);
        let issued = state.pending.remove(&bootstrap)?;
        if now.millis_since(issued) >= BOOTSTRAP_EXPIRES_MS
            || state.sessions.len() >= MAX_LIVE_SESSIONS
        {
            return None;
        }
        let cookie = loop {
            let candidate = mint_secret();
            if !state.sessions.contains_key(&digest(candidate.as_bytes())) {
                break candidate;
            }
        };
        let csrf = mint_secret();
        state.sessions.insert(
            digest(cookie.as_bytes()),
            Session {
                revoked: Arc::new(AtomicBool::new(false)),
                csrf: Some(digest(csrf.as_bytes())),
                created: now,
                last_used: now,
                websockets: 0,
                terminal: None,
                recovery_pending_until: None,
            },
        );
        Some(Exchange { cookie, csrf })
    }

    /// Rotate the CSRF carrier for one live, exact-origin session.
    #[must_use]
    pub(crate) fn recover(&self, host: &str, origin: &str, cookie: &str) -> Option<Recovery> {
        if !self.0.authority_matches(host, origin) || !self.0.valid.load(Ordering::Acquire) {
            return None;
        }
        let cookie = digest(cookie.as_bytes());
        let registry::Locked { mut state, now } = self.0.registry.lock_at_now();
        if !self.0.valid.load(Ordering::Acquire) {
            return None;
        }
        state.prune(now);
        let session = state.sessions.get_mut(&cookie)?;
        if session.websockets != 0
            || session.terminal.is_some()
            || session
                .recovery_pending_until
                .is_some_and(|until| now < until)
        {
            return None;
        }
        let csrf = mint_secret();
        session.csrf = Some(digest(csrf.as_bytes()));
        session.last_used = now;
        session.recovery_pending_until = Some(now.saturating_add_millis(RECOVERY_WINDOW_MS));
        Some(Recovery { csrf })
    }

    /// End this run's authority before its listener and connection owners are released.
    pub(crate) fn invalidate(&self) {
        let mut state = self.0.registry.lock();
        self.0.valid.store(false, Ordering::Release);
        for session in state.sessions.values() {
            session.revoked.store(true, Ordering::Release);
        }
        state.pending.clear();
        state.sessions.clear();
        state.inflight.clear();
    }
}

/// The Floor sessions and the run-local counts that follow them, which `dudes upgrade` carries
/// across its restart through `floor.session.export` and `floor.session.import`.
///
/// Lock order is the session registry, then a count's own lock. No count's lock is held while
/// the registry is taken.
#[derive(Clone)]
pub(crate) struct FloorCarry {
    sessions: FloorSessions,
    tickets: Option<Arc<TicketLimits>>,
    checks: CheckRuns,
}

impl FloorCarry {
    pub(crate) const fn new(
        sessions: FloorSessions,
        tickets: Option<Arc<TicketLimits>>,
        checks: CheckRuns,
    ) -> Self {
        Self {
            sessions,
            tickets,
            checks,
        }
    }

    pub(crate) const fn sessions(&self) -> &FloorSessions {
        &self.sessions
    }

    /// Every live session as this run holds it. Nothing is refreshed, and no cookie, CSRF secret,
    /// pending bootstrap, terminal grant or reservation leaves.
    ///
    /// A run with no identity exports nothing: an import could not tell its own run from another.
    pub(crate) fn export(&self, run: Option<&str>) -> SessionExport {
        let inner = &self.sessions.0;
        let mut exported = SessionExport {
            run: run.unwrap_or_default().to_owned(),
            origin: inner.origin.clone(),
            sessions: Vec::new(),
        };
        if run.is_none() || !inner.valid.load(Ordering::Acquire) {
            return exported;
        }
        let registry::Locked { mut state, now } = inner.registry.lock_at_now();
        if !inner.valid.load(Ordering::Acquire) {
            return exported;
        }
        state.prune(now);
        exported.sessions = state
            .sessions
            .iter()
            .map(|(cookie, session)| {
                let (ticket_creates, ticket_assigns) = self
                    .tickets
                    .as_ref()
                    .map_or((0, 0), |tickets| tickets.counts(*cookie));
                ExportedSession {
                    digest: Hex32::new(*cookie),
                    created: session.created,
                    last_used: session.last_used,
                    ticket_creates,
                    ticket_assigns,
                    check_runs: self.checks.started(Hex32::new(*cookie)),
                }
            })
            .collect();
        exported
    }

    /// Admit the sessions an earlier run exported, and say how many were admitted. Each one
    /// becomes the session it was, except that it matches no CSRF secret until recovery issues
    /// one. A run takes one import, from a run other than itself, and any attempt that passes the run check uses it up,
    /// whatever else it is refused for.
    pub(crate) fn import(&self, run: Option<&str>, exported: &SessionExport) -> u32 {
        let Some(run) = run else {
            return 0;
        };
        let inner = &self.sessions.0;
        let registry::Locked { mut state, now } = inner.registry.lock_at_now();
        if !inner.valid.load(Ordering::Acquire) || state.imported || exported.run == run {
            return 0;
        }
        state.imported = true;
        if exported.origin != inner.origin {
            return 0;
        }
        state.prune(now);
        let mut restored = 0;
        for entry in &exported.sessions {
            let key = entry.digest.bytes();
            let session = Session {
                revoked: Arc::new(AtomicBool::new(false)),
                csrf: None,
                created: entry.created,
                last_used: entry.last_used,
                websockets: 0,
                terminal: None,
                recovery_pending_until: None,
            };
            if now.millis_since(entry.created) < 0
                || now.millis_since(entry.last_used) < 0
                || !session.live_at(now)
                || state.sessions.contains_key(&key)
                || state.sessions.len() >= MAX_LIVE_SESSIONS
            {
                continue;
            }
            if let Some(tickets) = &self.tickets {
                tickets.seed(key, entry.ticket_creates, entry.ticket_assigns);
            }
            self.checks.seed(entry.digest, entry.check_runs);
            state.sessions.insert(key, session);
            restored += 1;
        }
        restored
    }
}

impl Inner {
    fn authority_matches(&self, host: &str, origin: &str) -> bool {
        host == self.host && origin == self.origin
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn mint_secret() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system's entropy source");
    base64url(&bytes)
}

fn base64url(bytes: &[u8; 32]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::with_capacity(43);
    for chunk in bytes.chunks(3) {
        let mut buffer = [0_u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let packed =
            (u32::from(buffer[0]) << 16) | (u32::from(buffer[1]) << 8) | u32::from(buffer[2]);
        for index in 0..=chunk.len() {
            let shift = 18 - 6 * index;
            output.push(char::from(ALPHABET[((packed >> shift) & 0x3f) as usize]));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::sync::mpsc;
    use std::thread;

    use dudes_supervisor::testing::FakeClock;

    use super::*;

    struct PermitProbeClock {
        inner: FakeClock,
        next_read: Mutex<Option<mpsc::Sender<()>>>,
    }

    impl Clock for PermitProbeClock {
        fn now(&self) -> Timestamp {
            // Read first: the value is fixed before the test can act on the signal.
            let now = self.inner.now();
            if let Some(signal) = self
                .next_read
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                let _ = signal.send(());
            }
            now
        }
    }

    #[test]
    fn expiry_while_waiting_for_permit_lock_refuses_the_late_caller() {
        let clock = Arc::new(PermitProbeClock {
            inner: FakeClock::frozen(0),
            next_read: Mutex::new(None),
        });
        let sessions = FloorSessions::new(
            "127.0.0.1:43123".parse().expect("loopback"),
            Arc::clone(&clock),
        );
        let host = "127.0.0.1:43123";
        let origin = "http://127.0.0.1:43123";
        let issued = sessions.issue().expect("bootstrap");
        let exchange = sessions
            .exchange(host, origin, issued.bootstrap.as_bytes())
            .expect("session");
        let identity = sessions
            .authorize_onboard(host, origin, &exchange.cookie, &exchange.csrf)
            .expect("admitted request");
        let first = identity.permit().expect("first append permit");
        let (sampled_tx, sampled_rx) = mpsc::channel();
        *clock
            .next_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(sampled_tx);
        let barrier = Arc::new(Barrier::new(2));
        let waiting = identity.clone();
        let started = Arc::clone(&barrier);
        let second = thread::spawn(move || {
            started.wait();
            waiting.permit().map(|_| ()).err()
        });
        barrier.wait();
        let sampled_while_held = sampled_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_ok();
        clock.inner.advance(ABSOLUTE_EXPIRES_MS);
        drop(first);
        let result = second.join().expect("waiting caller");
        assert!(
            sampled_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .is_ok()
                || sampled_while_held,
            "positive control: the waiting call did read the clock"
        );
        assert!(
            !sampled_while_held,
            "permit clock was read before the registry lock"
        );
        assert_eq!(
            result,
            Some(PermitFailure::Expired),
            "expired session cannot append"
        );
    }

    #[test]
    fn expired_same_key_reservation_waiter_cannot_receive_pending() {
        let clock = Arc::new(PermitProbeClock {
            inner: FakeClock::frozen(0),
            next_read: Mutex::new(None),
        });
        let sessions = FloorSessions::new(
            "127.0.0.1:43125".parse().expect("loopback"),
            Arc::clone(&clock),
        );
        let host = "127.0.0.1:43125";
        let origin = "http://127.0.0.1:43125";
        let first = sessions.issue().expect("first bootstrap");
        let first = sessions
            .exchange(host, origin, first.bootstrap.as_bytes())
            .expect("first session");
        let identity = sessions
            .authorize_onboard(host, origin, &first.cookie, &first.csrf)
            .expect("first admitted identity");
        let attempt = Hex32::new([0; 32]);
        let digest = Hex32::new([1; 32]);
        let ReserveResult::Owner(_owner) = identity.reserve(ActionKind::Onboard, attempt, digest)
        else {
            panic!("initial attempt reservation");
        };
        let second = sessions.issue().expect("other bootstrap");
        let second = sessions
            .exchange(host, origin, second.bootstrap.as_bytes())
            .expect("other session");
        let holder = sessions
            .authorize_onboard(host, origin, &second.cookie, &second.csrf)
            .expect("other admitted identity");
        let held = holder.permit().expect("other session holds registry lock");
        let (sampled_tx, sampled_rx) = mpsc::channel();
        *clock
            .next_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(sampled_tx);
        let barrier = Arc::new(Barrier::new(2));
        let waiting = identity.clone();
        let started = Arc::clone(&barrier);
        let follower = thread::spawn(move || {
            started.wait();
            matches!(
                waiting.reserve(ActionKind::Onboard, attempt, digest),
                ReserveResult::Forbidden
            )
        });
        barrier.wait();
        let sampled_while_held = sampled_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_ok();
        clock.inner.advance(ABSOLUTE_EXPIRES_MS);
        drop(held);
        let refused = follower.join().expect("follower answer");
        assert!(
            sampled_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .is_ok()
                || sampled_while_held,
            "positive control: follower read the clock"
        );
        assert!(
            !sampled_while_held,
            "reservation sampled expiry before the registry lock"
        );
        assert!(
            refused,
            "expired same-key follower cannot receive Pending/typed 202"
        );
    }

    #[test]
    fn expired_onboarding_entry_waiter_is_not_admitted_after_lock_release() {
        let clock = Arc::new(PermitProbeClock {
            inner: FakeClock::frozen(0),
            next_read: Mutex::new(None),
        });
        let sessions = FloorSessions::new(
            "127.0.0.1:43126".parse().expect("loopback"),
            Arc::clone(&clock),
        );
        let host = "127.0.0.1:43126";
        let origin = "http://127.0.0.1:43126";
        let first = sessions.issue().expect("first bootstrap");
        let first = sessions
            .exchange(host, origin, first.bootstrap.as_bytes())
            .expect("first session");
        let second = sessions.issue().expect("other bootstrap");
        let second = sessions
            .exchange(host, origin, second.bootstrap.as_bytes())
            .expect("other session");
        let holder = sessions
            .authorize_onboard(host, origin, &second.cookie, &second.csrf)
            .expect("other admitted identity");
        let held = holder.permit().expect("other session holds registry lock");
        let (sampled_tx, sampled_rx) = mpsc::channel();
        *clock
            .next_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(sampled_tx);
        let barrier = Arc::new(Barrier::new(2));
        let waiting = sessions.clone();
        let started = Arc::clone(&barrier);
        let follower = thread::spawn(move || {
            started.wait();
            waiting
                .authorize_onboard(host, origin, &first.cookie, &first.csrf)
                .is_none()
        });
        barrier.wait();
        let sampled_while_held = sampled_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_ok();
        clock.inner.advance(ABSOLUTE_EXPIRES_MS);
        drop(held);
        let refused = follower.join().expect("entry answer");
        assert!(
            sampled_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .is_ok()
                || sampled_while_held,
            "positive control: entry read the clock"
        );
        assert!(
            !sampled_while_held,
            "entry sampled expiry before the registry lock"
        );
        assert!(refused, "expired cookie cannot enter browser onboarding");
    }

    #[test]
    fn expired_catalog_reader_after_registry_lock_wait_is_not_valid() {
        let clock = Arc::new(PermitProbeClock {
            inner: FakeClock::frozen(0),
            next_read: Mutex::new(None),
        });
        let sessions = FloorSessions::new(
            "127.0.0.1:43127".parse().expect("loopback"),
            Arc::clone(&clock),
        );
        let host = "127.0.0.1:43127";
        let origin = "http://127.0.0.1:43127";
        let first = sessions.issue().expect("first bootstrap");
        let first = sessions
            .exchange(host, origin, first.bootstrap.as_bytes())
            .expect("first session");
        let identity = sessions
            .authorize_onboard(host, origin, &first.cookie, &first.csrf)
            .expect("first admitted identity");
        let second = sessions.issue().expect("other bootstrap");
        let second = sessions
            .exchange(host, origin, second.bootstrap.as_bytes())
            .expect("other session");
        let holder = sessions
            .authorize_onboard(host, origin, &second.cookie, &second.csrf)
            .expect("other admitted identity");
        let held = holder.permit().expect("other session holds registry lock");
        let (sampled_tx, sampled_rx) = mpsc::channel();
        *clock
            .next_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(sampled_tx);
        let barrier = Arc::new(Barrier::new(2));
        let waiting = identity.clone();
        let started = Arc::clone(&barrier);
        let reader = thread::spawn(move || {
            started.wait();
            waiting.is_valid()
        });
        barrier.wait();
        let sampled_while_held = sampled_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_ok();
        clock.inner.advance(ABSOLUTE_EXPIRES_MS);
        drop(held);
        let still_valid = reader.join().expect("catalog reader answer");
        assert!(
            sampled_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .is_ok()
                || sampled_while_held,
            "positive control: catalog reader read the clock"
        );
        assert!(
            !sampled_while_held,
            "catalog reader sampled expiry before the registry lock (still_valid={still_valid})"
        );
        assert!(
            !still_valid,
            "expired session cannot read the onboarding catalog"
        );
    }

    const PROBE_HOST: &str = "127.0.0.1:43128";
    const PROBE_ORIGIN: &str = "http://127.0.0.1:43128";

    fn probe_registry() -> (Arc<PermitProbeClock>, FloorSessions) {
        let clock = Arc::new(PermitProbeClock {
            inner: FakeClock::frozen(0),
            next_read: Mutex::new(None),
        });
        let sessions =
            FloorSessions::new(PROBE_HOST.parse().expect("loopback"), Arc::clone(&clock));
        (clock, sessions)
    }

    fn probe_session(sessions: &FloorSessions) -> Exchange {
        let issued = sessions.issue().expect("bootstrap");
        sessions
            .exchange(PROBE_HOST, PROBE_ORIGIN, issued.bootstrap.as_bytes())
            .expect("live session")
    }

    /// Another live session, whose append permit will hold the registry lock.
    fn probe_holder(sessions: &FloorSessions) -> SessionIdentity {
        let other = probe_session(sessions);
        sessions
            .authorize_onboard(PROBE_HOST, PROBE_ORIGIN, &other.cookie, &other.csrf)
            .expect("other admitted identity")
    }

    /// Run `call` while `held` owns the registry lock, advance the clock by `advance` during the
    /// wait, then release the lock. Returns the call's value and whether it read the clock before
    /// the release.
    fn behind_permit<T: Send>(
        clock: &PermitProbeClock,
        held: MutationPermit<'_>,
        advance: i64,
        call: impl FnOnce() -> T + Send,
    ) -> (T, bool) {
        let (sampled_tx, sampled_rx) = mpsc::channel();
        *clock
            .next_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(sampled_tx);
        let barrier = Barrier::new(2);
        thread::scope(|scope| {
            let started = &barrier;
            let waiter = scope.spawn(move || {
                started.wait();
                call()
            });
            barrier.wait();
            let sampled_while_held = sampled_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_ok();
            clock.inner.advance(advance);
            drop(held);
            let value = waiter.join().expect("waiting caller");
            assert!(
                sampled_rx
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .is_ok()
                    || sampled_while_held,
                "positive control: the waiting call read the clock"
            );
            (value, sampled_while_held)
        })
    }

    #[test]
    fn expired_bootstrap_waiter_cannot_exchange_after_registry_lock_wait() {
        let (clock, sessions) = probe_registry();
        let pending = sessions.issue().expect("pending bootstrap");
        let holder = probe_holder(&sessions);
        let held = holder.permit().expect("other session holds registry lock");
        let (exchanged, sampled_while_held) =
            behind_permit(&clock, held, BOOTSTRAP_EXPIRES_MS, || {
                sessions
                    .exchange(PROBE_HOST, PROBE_ORIGIN, pending.bootstrap.as_bytes())
                    .is_some()
            });
        assert!(
            !sampled_while_held,
            "exchange sampled expiry before the registry lock"
        );
        assert!(
            !exchanged,
            "a bootstrap that expired during the wait cannot exchange into a live session"
        );
    }

    #[test]
    fn idle_reader_after_registry_lock_wait_is_not_authorized() {
        let (clock, sessions) = probe_registry();
        let reader = probe_session(&sessions);
        let holder = probe_holder(&sessions);
        let held = holder.permit().expect("other session holds registry lock");
        let authorizer = sessions.authorizer();
        let (admitted, sampled_while_held) = behind_permit(&clock, held, IDLE_EXPIRES_MS, || {
            authorizer
                .authorize_read(PROBE_HOST, PROBE_ORIGIN, &reader.cookie, &reader.csrf)
                .is_some()
        });
        assert!(
            !sampled_while_held,
            "read authorization sampled expiry before the registry lock"
        );
        assert!(
            !admitted,
            "a session that went idle during the wait cannot read"
        );
    }

    #[test]
    fn idle_stream_opener_after_registry_lock_wait_is_not_authorized() {
        let (clock, sessions) = probe_registry();
        let browser = probe_session(&sessions);
        let holder = probe_holder(&sessions);
        let held = holder.permit().expect("other session holds registry lock");
        let authorizer = sessions.authorizer();
        let (opened, sampled_while_held) = behind_permit(&clock, held, IDLE_EXPIRES_MS, || {
            authorizer
                .authorize_stream(PROBE_HOST, PROBE_ORIGIN, &browser.cookie, &browser.csrf)
                .is_some()
        });
        assert!(
            !sampled_while_held,
            "stream authorization sampled expiry before the registry lock"
        );
        assert!(
            !opened,
            "a session that went idle during the wait cannot open a stream"
        );
    }

    #[test]
    fn idle_stream_after_registry_lock_wait_is_not_valid() {
        let (clock, sessions) = probe_registry();
        let owner = probe_session(&sessions);
        let stream = sessions
            .authorizer()
            .authorize_stream(PROBE_HOST, PROBE_ORIGIN, &owner.cookie, &owner.csrf)
            .expect("one stream slot");
        let holder = probe_holder(&sessions);
        let held = holder.permit().expect("other session holds registry lock");
        let (valid, sampled_while_held) =
            behind_permit(&clock, held, IDLE_EXPIRES_MS, || stream.is_valid());
        assert!(
            !sampled_while_held,
            "stream validity sampled expiry before the registry lock"
        );
        assert!(
            !valid,
            "a stream whose session went idle during the wait is not valid"
        );
    }

    #[test]
    fn idle_stream_after_registry_lock_wait_cannot_touch_subscribe() {
        let (clock, sessions) = probe_registry();
        let owner = probe_session(&sessions);
        let stream = sessions
            .authorizer()
            .authorize_stream(PROBE_HOST, PROBE_ORIGIN, &owner.cookie, &owner.csrf)
            .expect("one stream slot");
        let holder = probe_holder(&sessions);
        let held = holder.permit().expect("other session holds registry lock");
        let (touched, sampled_while_held) =
            behind_permit(&clock, held, IDLE_EXPIRES_MS, || stream.touch_subscribe());
        assert!(
            !sampled_while_held,
            "subscribe touch sampled expiry before the registry lock"
        );
        assert!(
            !touched,
            "a stream whose session went idle during the wait cannot refresh its idle deadline"
        );
    }

    #[test]
    fn absolute_expired_recovery_after_registry_lock_wait_is_refused() {
        let (clock, sessions) = probe_registry();
        let recovering = probe_session(&sessions);
        let authorizer = sessions.authorizer();
        for _ in 0..16 {
            clock.inner.advance(29 * 60_000);
            assert!(
                authorizer
                    .authorize_read(
                        PROBE_HOST,
                        PROBE_ORIGIN,
                        &recovering.cookie,
                        &recovering.csrf
                    )
                    .is_some(),
                "activity keeps the idle deadline fresh"
            );
        }
        // One minute short of the absolute bound, fifteen minutes idle.
        clock
            .inner
            .advance(ABSOLUTE_EXPIRES_MS - 16 * 29 * 60_000 - 60_000);
        let holder = probe_holder(&sessions);
        let held = holder.permit().expect("other session holds registry lock");
        let (recovered, sampled_while_held) = behind_permit(&clock, held, 60_000, || {
            sessions
                .recover(PROBE_HOST, PROBE_ORIGIN, &recovering.cookie)
                .is_some()
        });
        assert!(
            !sampled_while_held,
            "recovery sampled expiry before the registry lock"
        );
        assert!(
            !recovered,
            "a session that reached its absolute bound during the wait cannot recover"
        );
    }

    fn live_session(
        clock: Arc<FakeClock>,
    ) -> (FloorSessions, &'static str, &'static str, Exchange) {
        let sessions =
            FloorSessions::new("127.0.0.1:43123".parse().expect("loopback address"), clock);
        let host = "127.0.0.1:43123";
        let origin = "http://127.0.0.1:43123";
        let issued = sessions.issue().expect("one bootstrap");
        let exchange = sessions
            .exchange(host, origin, issued.bootstrap.as_bytes())
            .expect("one live session");
        (sessions, host, origin, exchange)
    }

    #[test]
    fn recover_installs_rotated_digest_before_return() {
        let clock = Arc::new(FakeClock::frozen(0));
        let (sessions, host, origin, exchange) = live_session(clock);

        let recovered = sessions
            .recover(host, origin, &exchange.cookie)
            .expect("the live zero-socket session recovers");

        assert_eq!(recovered.csrf.len(), 43);
        assert!(
            sessions
                .authorizer()
                .authorize_read(host, origin, &exchange.cookie, &recovered.csrf)
                .is_some(),
            "the returned nonce is installed before it can cross the response seam"
        );
    }

    #[test]
    fn recover_retries_wait_for_delayed_socket_drop() {
        let clock = Arc::new(FakeClock::frozen(0));
        let (sessions, host, origin, exchange) = live_session(Arc::clone(&clock));
        let held = sessions
            .authorizer()
            .authorize_stream(host, origin, &exchange.cookie, &exchange.csrf)
            .expect("the old page owns one socket");

        for delay in [0, 250, 500] {
            clock.advance(delay);
            assert!(
                sessions.recover(host, origin, &exchange.cookie).is_none(),
                "recovery waits while the old socket is still owned"
            );
        }
        drop(held);

        let recovered = sessions
            .recover(host, origin, &exchange.cookie)
            .expect("the first retry after drop wins");
        let winner = sessions
            .authorizer()
            .authorize_stream(host, origin, &exchange.cookie, &recovered.csrf)
            .expect("the winner opens with its rotated nonce");
        for delay in [1_000, 2_000, 4_000] {
            clock.advance(delay);
            assert!(
                sessions.recover(host, origin, &exchange.cookie).is_none(),
                "loser retries cannot rotate underneath the winner"
            );
        }
        drop(winner);
    }

    #[test]
    fn recover_serializes_simultaneous_callers() {
        let clock = Arc::new(FakeClock::frozen(0));
        let (sessions, host, origin, exchange) = live_session(Arc::clone(&clock));
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let sessions = sessions.clone();
            let barrier = Arc::clone(&barrier);
            let cookie = exchange.cookie.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                sessions.recover(host, origin, &cookie)
            }));
        }
        barrier.wait();
        let recovered: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().expect("recovery worker"))
            .collect();
        assert_eq!(
            recovered.iter().filter(|result| result.is_some()).count(),
            1,
            "one lock admission produces one winning page"
        );

        clock.advance(1_999);
        assert!(sessions.recover(host, origin, &exchange.cookie).is_none());
        clock.advance(1);
        assert!(
            sessions.recover(host, origin, &exchange.cookie).is_some(),
            "an unused winner can be replaced at window equality"
        );
    }

    #[test]
    fn recover_rejects_old_csrf_after_rotation() {
        let clock = Arc::new(FakeClock::frozen(0));
        let (sessions, host, origin, exchange) = live_session(clock);
        let recovered = sessions
            .recover(host, origin, &exchange.cookie)
            .expect("recovery rotates the nonce");

        assert!(
            sessions
                .authorizer()
                .authorize_stream(host, origin, &exchange.cookie, &exchange.csrf)
                .is_none(),
            "the previous nonce fails immediately"
        );
        assert!(
            sessions
                .authorizer()
                .authorize_stream(host, origin, &exchange.cookie, &recovered.csrf)
                .is_some(),
            "the returned nonce is the only current nonce"
        );
    }

    #[test]
    fn recover_winner_stream_clears_pending_window() {
        let clock = Arc::new(FakeClock::frozen(0));
        let (sessions, host, origin, exchange) = live_session(clock);
        let recovered = sessions
            .recover(host, origin, &exchange.cookie)
            .expect("one recovery");
        let winner = sessions
            .authorizer()
            .authorize_stream(host, origin, &exchange.cookie, &recovered.csrf)
            .expect("the recovery winner opens");
        drop(winner);

        assert!(
            sessions.recover(host, origin, &exchange.cookie).is_some(),
            "a successful winner clears serialization before two seconds"
        );
    }

    #[test]
    fn recover_churn_preserves_absolute_age_capacity_and_two_slots() {
        let clock = Arc::new(FakeClock::frozen(0));
        let (sessions, host, origin, exchange) = live_session(Arc::clone(&clock));
        let mut csrf = exchange.csrf;

        for _ in 0..3 {
            clock.advance(2_000);
            csrf = sessions
                .recover(host, origin, &exchange.cookie)
                .expect("zero-socket churn stays in one session row")
                .csrf;
        }
        for _ in 1..MAX_LIVE_SESSIONS {
            let issued = sessions.issue().expect("remaining session capacity");
            assert!(
                sessions
                    .exchange(host, origin, issued.bootstrap.as_bytes())
                    .is_some(),
                "recovery did not grow the live-session table"
            );
        }
        let overflow = sessions.issue().expect("one pending bootstrap");
        assert!(
            sessions
                .exchange(host, origin, overflow.bootstrap.as_bytes())
                .is_none(),
            "four live sessions remain the bound"
        );

        let first = sessions
            .authorizer()
            .authorize_stream(host, origin, &exchange.cookie, &csrf)
            .expect("first overlap slot");
        let second = sessions
            .authorizer()
            .authorize_stream(host, origin, &exchange.cookie, &csrf)
            .expect("second overlap slot");
        assert!(
            sessions
                .authorizer()
                .authorize_stream(host, origin, &exchange.cookie, &csrf)
                .is_none(),
            "recovery does not expand the two-slot bound"
        );
        drop(first);
        drop(second);

        for _ in 0..16 {
            clock.advance(29 * 60_000);
            let _ = sessions
                .recover(host, origin, &exchange.cookie)
                .expect("activity below both lifetime bounds");
        }
        clock.advance(16 * 60_000 - 6_000);
        assert!(
            sessions.recover(host, origin, &exchange.cookie).is_none(),
            "recovery never resets the eight-hour creation boundary"
        );
    }

    #[test]
    fn recover_refuses_idle_absolute_and_invalidated_with_fresh_controls() {
        let idle_clock = Arc::new(FakeClock::frozen(0));
        let (idle, host, origin, idle_exchange) = live_session(Arc::clone(&idle_clock));
        idle_clock.advance(30 * 60_000);
        assert!(idle.recover(host, origin, &idle_exchange.cookie).is_none());
        let fresh = idle.issue().expect("fresh bootstrap beside idle refusal");
        assert!(
            idle.exchange(host, origin, fresh.bootstrap.as_bytes())
                .is_some()
        );

        let absolute_clock = Arc::new(FakeClock::frozen(0));
        let (absolute, host, origin, absolute_exchange) = live_session(Arc::clone(&absolute_clock));
        for _ in 0..16 {
            absolute_clock.advance(29 * 60_000);
            assert!(
                absolute
                    .recover(host, origin, &absolute_exchange.cookie)
                    .is_some()
            );
        }
        absolute_clock.advance(16 * 60_000);
        assert!(
            absolute
                .recover(host, origin, &absolute_exchange.cookie)
                .is_none()
        );
        let fresh = absolute
            .issue()
            .expect("fresh bootstrap beside absolute refusal");
        assert!(
            absolute
                .exchange(host, origin, fresh.bootstrap.as_bytes())
                .is_some()
        );

        let ended_clock = Arc::new(FakeClock::frozen(0));
        let (ended, host, origin, ended_exchange) = live_session(Arc::clone(&ended_clock));
        ended.invalidate();
        assert!(
            ended.0.registry.lock().sessions.is_empty(),
            "invalidation carries no session row into ended run state"
        );
        assert!(
            ended
                .recover(host, origin, &ended_exchange.cookie)
                .is_none()
        );
        let (successor, host, origin, successor_exchange) = live_session(ended_clock);
        assert!(
            successor
                .authorizer()
                .authorize_read(
                    host,
                    origin,
                    &successor_exchange.cookie,
                    &successor_exchange.csrf,
                )
                .is_some(),
            "a fresh successor bootstrap succeeds beside invalidation"
        );
    }

    #[test]
    fn live_and_pending_tables_are_bounded_at_four() {
        let clock = Arc::new(FakeClock::frozen(0));
        let sessions = FloorSessions::new(
            "127.0.0.1:43123".parse().expect("loopback address"),
            Arc::clone(&clock),
        );
        let host = "127.0.0.1:43123";
        let origin = "http://127.0.0.1:43123";

        for _ in 0..MAX_LIVE_SESSIONS {
            let issued = sessions.issue().expect("one bounded bootstrap");
            assert!(
                sessions
                    .exchange(host, origin, issued.bootstrap.as_bytes())
                    .is_some()
            );
        }
        let fifth = sessions.issue().expect("a pending bootstrap still fits");
        assert!(
            sessions
                .exchange(host, origin, fifth.bootstrap.as_bytes())
                .is_none(),
            "a fifth live session is refused"
        );

        for _ in 0..MAX_PENDING_BOOTSTRAPS {
            sessions.issue().expect("one of four pending bootstraps");
        }
        assert_eq!(sessions.issue(), Err(IssueError::Capacity));
        clock.advance(BOOTSTRAP_EXPIRES_MS);
        assert!(sessions.issue().is_ok(), "expiry releases pending capacity");
    }
}

#[cfg(test)]
mod carry_tests {
    use dudes_supervisor::testing::FakeClock;

    use super::*;
    use crate::floor_ticket::Counted;

    const HOST: &str = "127.0.0.1:43211";
    const ORIGIN: &str = "http://127.0.0.1:43211";
    const OLD_RUN: &str = "run-old-0";
    const NEW_RUN: &str = "run-new-0";

    struct Run {
        carry: FloorCarry,
        tickets: Arc<TicketLimits>,
        checks: CheckRuns,
    }

    fn run(clock: &Arc<FakeClock>, host: &str) -> Run {
        let tickets = Arc::new(TicketLimits::new());
        let checks = CheckRuns::default();
        let sessions = FloorSessions::new(host.parse().expect("loopback"), Arc::clone(clock));
        Run {
            carry: FloorCarry::new(sessions, Some(Arc::clone(&tickets)), checks.clone()),
            tickets,
            checks,
        }
    }

    fn sign_in(run: &Run) -> Exchange {
        let issued = run.carry.sessions().issue().expect("bootstrap");
        run.carry
            .sessions()
            .exchange(HOST, ORIGIN, issued.bootstrap.as_bytes())
            .expect("session")
    }

    fn key(exchange: &Exchange) -> [u8; 32] {
        digest(exchange.cookie.as_bytes())
    }

    fn created(run: &Run, exchange: &Exchange, creates: u32, assigns: u32, checks: u32) {
        for (counted, times) in [(Counted::Create, creates), (Counted::Assign, assigns)] {
            for _ in 0..times {
                let turn = run
                    .tickets
                    .begin(key(exchange), Some(counted))
                    .expect("admitted");
                turn.succeeded();
            }
        }
        run.checks.seed(Hex32::new(key(exchange)), checks);
    }

    fn can_read(run: &Run, exchange: &Exchange, csrf: &str) -> bool {
        run.carry
            .sessions()
            .authorizer()
            .authorize_read(HOST, ORIGIN, &exchange.cookie, csrf)
            .is_some()
    }

    #[test]
    fn export_names_digests_times_and_counts_and_nothing_secret() {
        let clock = Arc::new(FakeClock::frozen(1_000));
        let old = run(&clock, HOST);
        let one = sign_in(&old);
        created(&old, &one, 3, 2, 5);
        clock.advance(60_000);

        let exported = old.carry.export(Some(OLD_RUN));
        let text = serde_json::to_string(&exported).expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        let top: Vec<_> = value.as_object().expect("object").keys().cloned().collect();
        assert_eq!(top, ["origin", "run", "sessions"]);
        let row = value["sessions"][0].as_object().expect("one row");
        let fields: Vec<_> = row.keys().map(String::as_str).collect();
        assert_eq!(
            fields,
            [
                "check_runs",
                "created",
                "digest",
                "last_used",
                "ticket_assigns",
                "ticket_creates"
            ]
        );
        assert_eq!(row["ticket_creates"], 3);
        assert_eq!(row["ticket_assigns"], 2);
        assert_eq!(row["check_runs"], 5);
        assert_eq!(row["created"], 1_000);
        assert_eq!(row["last_used"], 1_000, "the export refreshes nothing");
        assert_eq!(value["run"], OLD_RUN);
        assert_eq!(value["origin"], ORIGIN);
        assert!(!text.contains(&one.cookie) && !text.contains(&one.csrf));
        let digest = row["digest"].as_str().expect("digest");
        assert!(
            !format!("{exported:?}").contains(digest),
            "Debug lists no digest"
        );
        assert_eq!(digest.len(), 64, "the digest, not the cookie");
    }

    #[test]
    fn an_import_restores_the_session_with_its_counts_and_no_csrf() {
        let clock = Arc::new(FakeClock::frozen(1_000));
        let old = run(&clock, HOST);
        let one = sign_in(&old);
        created(&old, &one, 3, 2, 5);
        let exported = old.carry.export(Some(OLD_RUN));
        old.carry.sessions().invalidate();

        let new = run(&clock, HOST);
        assert_eq!(new.carry.import(Some(NEW_RUN), &exported), 1);

        assert!(
            !can_read(&new, &one, &one.csrf),
            "the old CSRF secret matches nothing"
        );
        assert!(!can_read(&new, &one, ""), "neither does an empty one");
        let recovery = new
            .carry
            .sessions()
            .recover(HOST, ORIGIN, &one.cookie)
            .expect("the cookie recovers");
        assert!(can_read(&new, &one, &recovery.csrf));
        assert_eq!(new.tickets.counts(key(&one)), (3, 2));
        assert_eq!(new.checks.started(Hex32::new(key(&one))), 5);
        let state = new.carry.sessions().0.registry.lock();
        let session = &state.sessions[&key(&one)];
        assert!(session.terminal.is_none());
        assert_eq!(session.websockets, 0);
    }

    #[test]
    fn the_original_sign_in_time_keeps_counting() {
        let now = 100_000_000;
        let clock = Arc::new(FakeClock::frozen(now));
        let cookie = "a-cookie-the-new-run-never-saw";
        let exported = SessionExport {
            run: OLD_RUN.to_owned(),
            origin: ORIGIN.to_owned(),
            sessions: vec![ExportedSession {
                digest: Hex32::new(digest(cookie.as_bytes())),
                created: Timestamp::from_unix_millis(now - (ABSOLUTE_EXPIRES_MS - 10_000)),
                last_used: Timestamp::from_unix_millis(now - 1_000),
                ticket_creates: 0,
                ticket_assigns: 0,
                check_runs: 0,
            }],
        };
        let new = run(&clock, HOST);
        assert_eq!(new.carry.import(Some(NEW_RUN), &exported), 1);
        clock.advance(10_001);
        assert!(
            new.carry.sessions().recover(HOST, ORIGIN, cookie).is_none(),
            "eight hours after the original sign-in the imported session is gone"
        );
    }

    #[test]
    fn an_import_refuses_or_skips_what_it_must() {
        let clock = Arc::new(FakeClock::frozen(10_000));
        let old = run(&clock, HOST);
        let one = sign_in(&old);
        let exported = old.carry.export(Some(OLD_RUN));

        assert_eq!(
            old.carry.import(Some(OLD_RUN), &exported),
            0,
            "the run that exported"
        );
        let new = run(&clock, HOST);
        assert_eq!(new.carry.import(Some(OLD_RUN), &exported), 0, "same run id");

        let first = new.carry.import(Some(NEW_RUN), &exported);
        assert_eq!(first, 1);
        let later = sign_in(&old);
        assert_eq!(
            new.carry
                .import(Some(NEW_RUN), &old.carry.export(Some(OLD_RUN))),
            0,
            "a second import admits nothing, not even a session the first one lacked"
        );
        assert!(
            new.carry
                .sessions()
                .recover(HOST, ORIGIN, &later.cookie)
                .is_none()
        );

        let other = run(&clock, HOST);
        let mut moved = exported.clone();
        moved.origin = "http://127.0.0.1:43212".to_owned();
        assert_eq!(
            other.carry.import(Some(NEW_RUN), &moved),
            0,
            "another origin"
        );
        assert!(
            other
                .carry
                .sessions()
                .recover(HOST, ORIGIN, &one.cookie)
                .is_none()
        );
    }

    #[test]
    fn a_live_digest_is_skipped_and_never_overwritten() {
        let clock = Arc::new(FakeClock::frozen(10_000));
        let new = run(&clock, HOST);
        let live = sign_in(&new);
        created(&new, &live, 1, 1, 1);
        let before = new.carry.export(Some(NEW_RUN)).sessions;
        let mut entry = before[0];
        entry.ticket_creates = 30;
        entry.created = Timestamp::from_unix_millis(5_000);
        let exported = SessionExport {
            run: OLD_RUN.to_owned(),
            origin: ORIGIN.to_owned(),
            sessions: vec![entry],
        };
        assert_eq!(new.carry.import(Some(NEW_RUN), &exported), 0);
        assert_eq!(new.carry.export(Some(NEW_RUN)).sessions, before);
        assert!(can_read(&new, &live, &live.csrf), "its secret still works");
    }

    #[test]
    fn expired_and_future_entries_are_dropped() {
        let now = 100_000_000;
        let clock = Arc::new(FakeClock::frozen(now));
        let entry = |created: i64, last_used: i64| ExportedSession {
            digest: Hex32::new([9; 32]),
            created: Timestamp::from_unix_millis(created),
            last_used: Timestamp::from_unix_millis(last_used),
            ticket_creates: 0,
            ticket_assigns: 0,
            check_runs: 0,
        };
        let restored = |entry: ExportedSession| {
            run(&clock, HOST).carry.import(
                Some(NEW_RUN),
                &SessionExport {
                    run: OLD_RUN.to_owned(),
                    origin: ORIGIN.to_owned(),
                    sessions: vec![entry],
                },
            )
        };
        assert_eq!(restored(entry(now - 1_000, now - 500)), 1, "control");
        assert_eq!(
            restored(entry(now - IDLE_EXPIRES_MS, now - IDLE_EXPIRES_MS)),
            0
        );
        assert_eq!(restored(entry(now - ABSOLUTE_EXPIRES_MS, now)), 0);
        assert_eq!(restored(entry(now - 1_000, now + 1)), 0);
        assert_eq!(restored(entry(now + 1, now + 1)), 0);
    }

    #[test]
    fn an_import_keeps_the_sessions_the_run_already_issued_within_the_cap() {
        let clock = Arc::new(FakeClock::frozen(1_000));
        let old = run(&clock, HOST);
        for _ in 0..MAX_LIVE_SESSIONS {
            sign_in(&old);
        }
        let exported = old.carry.export(Some(OLD_RUN));
        assert_eq!(exported.sessions.len(), MAX_LIVE_SESSIONS);

        let new = run(&clock, HOST);
        let issued = [sign_in(&new), sign_in(&new), sign_in(&new)];
        assert_eq!(new.carry.import(Some(NEW_RUN), &exported), 1);
        for exchange in &issued {
            assert!(can_read(&new, exchange, &exchange.csrf));
        }
    }

    #[test]
    fn a_run_that_never_imports_restores_nothing_and_an_ended_run_exports_nothing() {
        let clock = Arc::new(FakeClock::frozen(1_000));
        let old = run(&clock, HOST);
        let one = sign_in(&old);
        old.carry.sessions().invalidate();
        assert!(old.carry.export(Some(OLD_RUN)).sessions.is_empty());
        let new = run(&clock, HOST);
        assert!(
            new.carry
                .sessions()
                .recover(HOST, ORIGIN, &one.cookie)
                .is_none(),
            "a plain restart signs everyone out"
        );
    }

    #[test]
    fn any_import_that_passes_the_run_check_uses_the_runs_one_chance() {
        let clock = Arc::new(FakeClock::frozen(1_000));
        let old = run(&clock, HOST);
        sign_in(&old);
        let exported = old.carry.export(Some(OLD_RUN));
        let new = run(&clock, HOST);
        let mut wrong = exported.clone();
        wrong.origin = "http://127.0.0.1:43212".to_owned();
        assert_eq!(new.carry.import(Some(NEW_RUN), &wrong), 0);
        assert_eq!(
            new.carry.import(Some(NEW_RUN), &exported),
            0,
            "a refused attempt still used the one import"
        );
    }

    #[test]
    fn a_run_with_no_identity_exports_nothing_and_imports_nothing() {
        let clock = Arc::new(FakeClock::frozen(1_000));
        let old = run(&clock, HOST);
        sign_in(&old);
        assert!(old.carry.export(None).sessions.is_empty());
        let exported = old.carry.export(Some(OLD_RUN));
        assert_eq!(
            exported.sessions.len(),
            1,
            "control: the same run exports with an identity"
        );
        let new = run(&clock, HOST);
        assert_eq!(new.carry.import(None, &exported), 0);
        assert_eq!(
            new.carry.import(Some(NEW_RUN), &exported),
            1,
            "a refused import without an identity did not use the run's chance"
        );
    }

    #[test]
    fn carried_allowances_refuse_with_their_literal_reason_after_an_import() {
        use crate::baseline::{Baselines, Limit, SESSION_BASELINES};
        use crate::floor_ticket::{MOST_ASSIGNS, MOST_CREATES};

        let clock = Arc::new(FakeClock::frozen(1_000));
        let old = run(&clock, HOST);
        let one = sign_in(&old);
        created(&old, &one, 3, 2, 5);
        let mut exported = old.carry.export(Some(OLD_RUN));
        exported.sessions[0].ticket_creates = MOST_CREATES;
        exported.sessions[0].ticket_assigns = MOST_ASSIGNS - 1;
        exported.sessions[0].check_runs = SESSION_BASELINES;

        let baselines = Baselines::default();
        let tickets = Arc::new(TicketLimits::new());
        let sessions = FloorSessions::new(HOST.parse().expect("loopback"), Arc::clone(&clock));
        let carry = FloorCarry::new(sessions, Some(Arc::clone(&tickets)), baselines.check_runs());
        let mut baselines = baselines;
        assert_eq!(carry.import(Some(NEW_RUN), &exported), 1);

        assert_eq!(
            tickets.begin(key(&one), Some(Counted::Create)).err(),
            Some("limit_reached"),
            "the create allowance was spent before the upgrade"
        );
        assert!(
            tickets.begin(key(&one), Some(Counted::Assign)).is_ok(),
            "control: an assign still has one left"
        );
        assert_eq!(
            baselines.start_browser(None, Hex32::new(key(&one))),
            Err(Limit::Exhausted),
            "the check-run cap was reached before the upgrade"
        );
    }
}
