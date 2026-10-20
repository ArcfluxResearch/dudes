//! Run-local authentication for the web Floor.
//!
//! The local RPC route and the loopback HTTP listener share this module. Raw bootstrap, cookie and
//! secret values leave only at their one required response; the retained state contains SHA-256
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

mod recovery;
pub(crate) use recovery::{FloorRecoveryAuthority, RecoveryGrants, RecoveryPermitFailure};

/// Cookie name used by the read-only Floor session.
pub const SESSION_COOKIE_NAME: &str = "dudes_floor_session";
/// Header carrying the session secret on every authenticated request that changes state.
pub const CSRF_HEADER_NAME: &str = "X-Dudes-CSRF";

const BOOTSTRAP_EXPIRES_MS: i64 = 60_000;
/// A session ends after this long without use.
const IDLE_EXPIRES_MS: i64 = 24 * 60 * 60_000;
/// A session ends this long after sign-in, however busy it was.
const ABSOLUTE_EXPIRES_MS: i64 = 30 * 24 * 60 * 60_000;
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
    /// How long the browser keeps the cookie, in seconds.
    pub(crate) cookie_max_age_s: i64,
}

impl fmt::Debug for Exchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Exchange")
            .field("cookie", &"<redacted>")
            .field("csrf", &"<redacted>")
            .finish()
    }
}

/// An unforgeable admission to the future read-only projection facade.
pub struct ReadOnlySession {
    _private: (),
}

/// One confirmed reader, retained without extending its idle deadline.
pub struct ReceiptSession {
    identity: SessionIdentity,
    csrf: [u8; 32],
}

impl fmt::Debug for ReceiptSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReceiptSession(<redacted>)")
    }
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
    pub(crate) fn validates_receipt(&self, reader: &ReceiptSession) -> bool {
        let identity = &reader.identity;
        if !Arc::ptr_eq(&self.0, &identity.inner) {
            return false;
        }
        let registry::Locked { state, now } = self.0.registry.lock_at_now();
        self.0.valid.load(Ordering::Acquire)
            && !identity.revoked.load(Ordering::Acquire)
            && state.sessions.get(&identity.cookie).is_some_and(|session| {
                Arc::ptr_eq(&session.revoked, &identity.revoked)
                    && session.csrf == Some(reader.csrf)
                    && session.live_at(now)
            })
    }

    /// Revoke one presented session under the registry lock; test-only caller access.
    #[doc(hidden)]
    pub fn revoke_for_test(&self, cookie: &str) -> bool {
        let cookie = digest(cookie.as_bytes());
        self.0.registry.lock().end(&cookie)
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

    /// Authenticate one engine sign-in upgrade and claim its session's one sign-in slot.
    #[must_use]
    pub(crate) fn authorize_login(
        &self,
        host: &str,
        origin: &str,
        cookie: &str,
        csrf: &str,
    ) -> Option<LoginSession> {
        let ((), cookie, revoked) = self.admit_socket(host, origin, cookie, csrf, |session| {
            (!session.login).then(|| session.login = true)
        })?;
        Some(LoginSession {
            inner: Arc::clone(&self.0),
            cookie,
            revoked,
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

/// One authenticated engine sign-in socket. A session holds at most one.
pub(crate) struct LoginSession {
    inner: Arc<Inner>,
    cookie: [u8; 32],
    revoked: Arc<AtomicBool>,
}

impl LoginSession {
    /// Whether the session is still live. Only typing advances its idle time, through [`Self::touch`].
    pub(crate) fn is_valid(&self) -> bool {
        self.check(false)
    }

    /// Count the person's own typing as session activity.
    pub(crate) fn touch(&self) -> bool {
        self.check(true)
    }

    fn check(&self, advance: bool) -> bool {
        if !self.inner.valid.load(Ordering::Acquire) || self.revoked.load(Ordering::Acquire) {
            return false;
        }
        let registry::Locked { mut state, now } = self.inner.registry.lock_at_now();
        state.prune(now);
        let Some(session) = state.sessions.get_mut(&self.cookie) else {
            return false;
        };
        if advance {
            session.last_used = now;
        }
        self.inner.valid.load(Ordering::Acquire)
    }
}

impl Drop for LoginSession {
    fn drop(&mut self) {
        if let Some(session) = self.inner.registry.lock().sessions.get_mut(&self.cookie) {
            session.login = false;
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
    Project,
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

    /// How long the browser should keep this session's cookie, or `None` once the session is gone.
    pub(crate) fn cookie_max_age_s(&self) -> Option<i64> {
        let registry::Locked { state, now } = self.inner.registry.lock_at_now();
        state
            .sessions
            .get(&self.cookie)
            .filter(|session| session.live_at(now))
            .map(|session| session.cookie_max_age_s(now))
    }

    /// Sign this session out: it leaves the table now, so its sockets close at their next check.
    pub(crate) fn end(&self) {
        self.inner.registry.lock().end(&self.cookie);
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
    /// The digest of the secret the page sends with every request that changes state. `None` for
    /// a session imported from an export that carried none, which can never act.
    csrf: Option<[u8; 32]>,
    created: Timestamp,
    last_used: Timestamp,
    websockets: usize,
    terminal: Option<TerminalSlot>,
    /// Whether this session holds its one engine sign-in socket.
    login: bool,
}

impl Session {
    fn live_at(&self, now: Timestamp) -> bool {
        now.millis_since(self.created) < ABSOLUTE_EXPIRES_MS
            && now.millis_since(self.last_used) < IDLE_EXPIRES_MS
    }

    /// How long the browser should keep this session's cookie: whichever of the two limits comes
    /// first, in whole seconds and never below one.
    fn cookie_max_age_s(&self, now: Timestamp) -> i64 {
        let idle = IDLE_EXPIRES_MS - now.millis_since(self.last_used);
        let absolute = ABSOLUTE_EXPIRES_MS - now.millis_since(self.created);
        (idle.min(absolute) / 1000).max(1)
    }
}

impl State {
    fn prune(&mut self, now: Timestamp) {
        self.pending
            .retain(|_, issued| now.millis_since(*issued) < BOOTSTRAP_EXPIRES_MS);
        self.sessions.retain(|_, session| session.live_at(now));
    }

    /// End one session: it leaves the table, and everything holding its revoked flag sees it.
    fn end(&mut self, key: &[u8; 32]) -> bool {
        self.sessions.remove(key).is_some_and(|session| {
            session.revoked.store(true, Ordering::Release);
            true
        })
    }

    /// End the session that was used longest ago. Equal uses go to the older sign-in, then to the
    /// smaller key, so the choice never depends on map order.
    fn end_least_recently_used(&mut self) {
        let oldest = self
            .sessions
            .iter()
            .min_by_key(|(key, session)| {
                (
                    session.last_used.unix_millis(),
                    session.created.unix_millis(),
                    **key,
                )
            })
            .map(|(key, _)| *key);
        if let Some(key) = oldest {
            self.end(&key);
        }
    }
}

impl FloorSessions {
    #[cfg(test)]
    pub(crate) fn registry_counts_for_test(&self) -> (usize, usize) {
        let state = self.0.registry.lock();
        (state.pending.len(), state.sessions.len())
    }

    /// Admit one exact-origin browser action — onboarding, an answer, a pause or a resume — and
    /// retain only its session digest.
    pub(crate) fn authorize_onboard(
        &self,
        host: &str,
        origin: &str,
        cookie: &str,
        csrf: &str,
    ) -> Option<SessionIdentity> {
        self.authenticate(host, origin, cookie, csrf, IdleClock::Advance)
    }

    /// Admit a request that asks about the session itself, and leave its idle deadline alone.
    pub(crate) fn authorize_standing(
        &self,
        host: &str,
        origin: &str,
        cookie: &str,
        csrf: &str,
    ) -> Option<SessionIdentity> {
        self.authenticate(host, origin, cookie, csrf, IdleClock::Hold)
    }

    pub(crate) fn authorize_receipt(
        &self,
        host: &str,
        origin: &str,
        cookie: &str,
        csrf: &str,
    ) -> Option<ReceiptSession> {
        self.authenticate(host, origin, cookie, csrf, IdleClock::Hold)
            .map(|identity| ReceiptSession {
                identity,
                csrf: digest(csrf.as_bytes()),
            })
    }

    fn authenticate(
        &self,
        host: &str,
        origin: &str,
        cookie: &str,
        csrf: &str,
        idle: IdleClock,
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
        if idle == IdleClock::Advance {
            state.prune(now);
        }
        let session = state.sessions.get_mut(&cookie)?;
        if session.csrf != Some(csrf)
            || !session.live_at(now)
            || session.revoked.load(Ordering::Acquire)
        {
            return None;
        }
        if idle == IdleClock::Advance {
            session.last_used = now;
        }
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
        // Pruning has dropped every expired link, so a link found here is good, and nothing after
        // this refuses: a full table makes room instead, so a link is spent only by a sign-in.
        state.pending.remove(&bootstrap)?;
        while state.sessions.len() >= MAX_LIVE_SESSIONS {
            state.end_least_recently_used();
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
                login: false,
            },
        );
        Some(Exchange {
            cookie,
            csrf,
            cookie_max_age_s: IDLE_EXPIRES_MS / 1000,
        })
    }

    /// Whether this run still honours sessions. Once it is stopping, an unknown session is not
    /// news about the session, because every session was just cleared.
    pub(crate) fn is_running(&self) -> bool {
        self.0.valid.load(Ordering::Acquire)
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

    /// Every live session as this run holds it. Nothing is refreshed, and no cookie, secret,
    /// pending bootstrap, terminal grant or reservation leaves: only their digests do.
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
                    secret: session.csrf.map(Hex32::new),
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
    /// becomes the session it was, and keeps acting with the secret its page already holds. A run takes one import, from a run other than itself, and any attempt that passes the run check uses it up,
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
                csrf: entry.secret.map(Hex32::bytes),
                created: entry.created,
                last_used: entry.last_used,
                websockets: 0,
                terminal: None,
                login: false,
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
                .is_some(),
            "a fifth sign-in makes room instead of being refused"
        );
        assert_eq!(sessions.registry_counts_for_test().1, MAX_LIVE_SESSIONS);

        for _ in 0..MAX_PENDING_BOOTSTRAPS {
            sessions.issue().expect("one of four pending bootstraps");
        }
        assert_eq!(sessions.issue(), Err(IssueError::Capacity));
        clock.advance(BOOTSTRAP_EXPIRES_MS);
        assert!(sessions.issue().is_ok(), "expiry releases pending capacity");
    }

    const HOUR_MS: i64 = 60 * 60_000;
    const LIFETIME_HOST: &str = "127.0.0.1:43124";
    const LIFETIME_ORIGIN: &str = "http://127.0.0.1:43124";

    fn lifetime_sessions() -> (Arc<FakeClock>, FloorSessions) {
        let clock = Arc::new(FakeClock::frozen(0));
        let sessions = FloorSessions::new(
            LIFETIME_HOST.parse().expect("loopback address"),
            Arc::clone(&clock),
        );
        (clock, sessions)
    }

    fn sign_in(sessions: &FloorSessions) -> Exchange {
        let issued = sessions.issue().expect("a bootstrap");
        sessions
            .exchange(LIFETIME_HOST, LIFETIME_ORIGIN, issued.bootstrap.as_bytes())
            .expect("a session")
    }

    fn acts(sessions: &FloorSessions, session: &Exchange) -> bool {
        sessions
            .authorize_onboard(
                LIFETIME_HOST,
                LIFETIME_ORIGIN,
                &session.cookie,
                &session.csrf,
            )
            .is_some()
    }

    #[test]
    fn a_fifth_sign_in_ends_the_session_used_longest_ago_and_spends_its_link_once() {
        let (clock, sessions) = lifetime_sessions();
        let mut live = Vec::new();
        for _ in 0..MAX_LIVE_SESSIONS {
            live.push(sign_in(&sessions));
            clock.advance(1_000);
        }
        assert!(
            acts(&sessions, &live[0]),
            "the oldest sign-in was used last"
        );
        clock.advance(1_000);

        let issued = sessions.issue().expect("a bootstrap");
        let fifth = sessions
            .exchange(LIFETIME_HOST, LIFETIME_ORIGIN, issued.bootstrap.as_bytes())
            .expect("a full table makes room and does not refuse");
        assert!(
            sessions
                .exchange(LIFETIME_HOST, LIFETIME_ORIGIN, issued.bootstrap.as_bytes())
                .is_none(),
            "the link was spent by the sign-in it earned"
        );
        assert!(!acts(&sessions, &live[1]), "the least recently used ended");
        for kept in [&live[0], &live[2], &live[3], &fifth] {
            assert!(acts(&sessions, kept));
        }
    }

    #[test]
    fn a_session_ends_after_a_day_idle_and_after_thirty_days_in_all() {
        let (clock, sessions) = lifetime_sessions();
        let idle = sign_in(&sessions);
        let busy = sign_in(&sessions);
        clock.advance(24 * HOUR_MS - 1);
        assert!(
            acts(&sessions, &busy),
            "a day less a millisecond is still live"
        );
        clock.advance(1);
        assert!(!acts(&sessions, &idle), "a day idle ends it");
        assert!(acts(&sessions, &busy), "use renewed the other");

        for _ in 0..30 {
            clock.advance(23 * HOUR_MS);
            assert!(acts(&sessions, &busy), "use keeps it for 30 days");
        }
        clock.advance(23 * HOUR_MS);
        assert!(
            !acts(&sessions, &busy),
            "thirty days in all end it, however busy"
        );
    }

    #[test]
    fn the_cookie_is_kept_for_the_sooner_of_the_two_limits() {
        let (clock, sessions) = lifetime_sessions();
        let session = sign_in(&sessions);
        assert_eq!(session.cookie_max_age_s, 24 * 3600);
        let identity = |s: &Exchange| {
            sessions
                .authorize_onboard(LIFETIME_HOST, LIFETIME_ORIGIN, &s.cookie, &s.csrf)
                .expect("live")
        };
        clock.advance(6 * HOUR_MS);
        assert_eq!(identity(&session).cookie_max_age_s(), Some(24 * 3600));
        for _ in 0..30 {
            clock.advance(23 * HOUR_MS);
            identity(&session);
        }
        clock.advance(HOUR_MS);
        let near_the_end = 30 * 24 * 3600 - (6 + 30 * 23 + 1) * 3600;
        assert_eq!(identity(&session).cookie_max_age_s(), Some(near_the_end));
    }

    #[test]
    fn signing_out_ends_the_session_and_closes_its_sockets() {
        let (_, sessions) = lifetime_sessions();
        let leaving = sign_in(&sessions);
        let staying = sign_in(&sessions);
        let socket = sessions
            .authorizer()
            .authorize_stream(
                LIFETIME_HOST,
                LIFETIME_ORIGIN,
                &leaving.cookie,
                &leaving.csrf,
            )
            .expect("a stream slot");
        let kept = sessions
            .authorizer()
            .authorize_stream(
                LIFETIME_HOST,
                LIFETIME_ORIGIN,
                &staying.cookie,
                &staying.csrf,
            )
            .expect("a stream slot");
        assert!(socket.is_valid() && kept.is_valid());

        sessions
            .authorize_standing(
                LIFETIME_HOST,
                LIFETIME_ORIGIN,
                &leaving.cookie,
                &leaving.csrf,
            )
            .expect("known")
            .end();

        assert!(!socket.is_valid(), "its socket closes at the next check");
        assert!(!acts(&sessions, &leaving), "its secret acts for no one");
        assert!(
            sessions
                .authorize_standing(
                    LIFETIME_HOST,
                    LIFETIME_ORIGIN,
                    &leaving.cookie,
                    &leaving.csrf
                )
                .is_none(),
            "the daemon now says the session is unknown"
        );
        assert!(kept.is_valid() && acts(&sessions, &staying));
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
                "secret",
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
        let secret = row["secret"].as_str().expect("the secret's digest");
        assert!(
            !format!("{exported:?}").contains(digest) && !format!("{exported:?}").contains(secret),
            "Debug lists no digest"
        );
        assert_eq!(digest.len(), 64, "the digest, not the cookie");
        assert_eq!(secret.len(), 64, "the digest, not the secret");
        assert_ne!(secret, digest);
    }

    #[test]
    fn an_import_restores_the_session_with_its_counts_and_its_secret() {
        let clock = Arc::new(FakeClock::frozen(1_000));
        let old = run(&clock, HOST);
        let one = sign_in(&old);
        created(&old, &one, 3, 2, 5);
        let exported = old.carry.export(Some(OLD_RUN));
        old.carry.sessions().invalidate();

        let new = run(&clock, HOST);
        assert_eq!(new.carry.import(Some(NEW_RUN), &exported), 1);

        assert!(
            can_read(&new, &one, &one.csrf),
            "the secret the page holds still works"
        );
        assert!(!can_read(&new, &one, ""), "an empty one does not");
        assert!(
            !can_read(&new, &one, &"A".repeat(43)),
            "nor does a wrong one"
        );
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
                secret: Some(Hex32::new(digest(b"the-secret"))),
                created: Timestamp::from_unix_millis(now - (ABSOLUTE_EXPIRES_MS - 10_000)),
                last_used: Timestamp::from_unix_millis(now - 1_000),
                ticket_creates: 0,
                ticket_assigns: 0,
                check_runs: 0,
            }],
        };
        let new = run(&clock, HOST);
        assert_eq!(new.carry.import(Some(NEW_RUN), &exported), 1);
        let authorizer = new.carry.sessions().authorizer();
        let admits = || {
            authorizer
                .authorize_read(HOST, ORIGIN, cookie, "the-secret")
                .is_some()
        };
        assert!(admits());
        clock.advance(10_001);
        assert!(
            !admits(),
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
        assert!(!can_read(&new, &later, &later.csrf));

        let other = run(&clock, HOST);
        let mut moved = exported.clone();
        moved.origin = "http://127.0.0.1:43212".to_owned();
        assert_eq!(
            other.carry.import(Some(NEW_RUN), &moved),
            0,
            "another origin"
        );
        assert!(!can_read(&other, &one, &one.csrf));
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
            secret: Some(Hex32::new([8; 32])),
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
            !can_read(&new, &one, &one.csrf),
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
