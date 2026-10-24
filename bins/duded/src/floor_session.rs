//! Authentication for the web Floor: sign-in links and the devices they sign in.
//!
//! The local RPC route and the loopback HTTP listener share this module. A browser that exchanges a
//! link becomes a device. The device table is a projection of journal records: every change to it
//! is handed to a [`DeviceSink`] to be made durable, and a restart seeds the table back from the
//! journal. Links live only in memory. Raw link and cookie values leave only at their one required
//! response; the retained state contains SHA-256 digests and each device's public key.
//!
//! A cookie names a device and opens nothing on its own. Every request also carries a signature
//! made with the device's key, which [`signature`] reads and the registry checks against the clock
//! and the device's memory of recent request ids. Stopping the owning listener invalidates and
//! clears the complete state before the run releases its transport.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

use dudes_proto::record::{DeviceId, FloorDeviceRemoval, Hex32};
use dudes_proto::rpc::floor::{
    ExportedSession, LinkPurpose, SESSION_EXPIRES_IN_S, SESSION_PRINTED_EXPIRES_IN_S,
    SessionExport, SessionIssueResult,
};
use dudes_supervisor::{Clock, DeviceRow, FloorDeviceChange};
use dudes_types::Timestamp;
use sha2::{Digest as _, Sha256};

use crate::baseline::CheckRuns;
use crate::floor_ticket::TicketLimits;

mod recovery;
pub(crate) mod signature;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) use recovery::{FloorRecoveryAuthority, RecoveryGrants, RecoveryPermitFailure};
use signature::{DeviceKey, Presented, Subject};

/// Cookie name used by the read-only Floor session.
pub const SESSION_COOKIE_NAME: &str = "dudes_floor_session";

const DAY_MS: i64 = 24 * 60 * 60_000;
/// A link made for `dudes open` lives this long.
const OPEN_LINK_MS: i64 = 60_000;
/// A link made to be printed and pasted lives this long.
const PRINTED_LINK_MS: i64 = 10 * 60_000;
/// A device ends after this long without use.
const IDLE_EXPIRES_MS: i64 = 30 * DAY_MS;
/// A device ends this long after sign-in, however busy it was.
const ABSOLUTE_EXPIRES_MS: i64 = 90 * DAY_MS;
/// A device that makes no request this long after signing in ends.
const NEVER_USED_EXPIRES_MS: i64 = DAY_MS;
/// Open links waiting to be exchanged. A full table refuses a new one.
const MAX_PENDING_OPEN_LINKS: usize = 4;
/// Printed links waiting to be exchanged. A new one replaces the oldest, so printing never
/// takes a slot `dudes open` needs.
const MAX_PENDING_PRINTED_LINKS: usize = 4;
const MAX_DEVICES: usize = 8;
/// The most often a device's use is journaled.
const USE_JOURNAL_MS: i64 = DAY_MS;
/// How many ended devices the daemon remembers well enough to say why one was signed out.
const MAX_REMEMBERED_ENDED: usize = 64;
/// The longest device name, in characters.
const MAX_NAME_CHARS: usize = 64;
const DEFAULT_NAME: &str = "Browser";
/// The most often an open stream on a visible page renews its session's idle time.
const WATCH_RENEWAL_MS: i64 = 60_000;
/// A signed request may be this far from the daemon's clock, either way.
const SIGNATURE_WINDOW_MS: i64 = 60_000;
/// A request id is refused for this long after it was used.
const REQUEST_MEMORY_MS: i64 = 2 * 60_000;
/// The most request ids a device's memory holds. A full memory refuses until ids age out.
const MAX_REQUEST_IDS: usize = 1024;
const MAX_WEBSOCKETS_PER_SESSION: usize = 2;

/// The issue route cannot mint another link while its bounded run-local table is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueError {
    /// Four still-live links to open already await exchange.
    Capacity,
    /// The listener that owned this run-local state has stopped.
    RunEnded,
}

impl fmt::Display for IssueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capacity => f.write_str("the Floor link table is full"),
            Self::RunEnded => f.write_str("the Floor session run has ended"),
        }
    }
}

/// How the device table reaches the journal and the open pages.
///
/// The daemon's implementation sends each change to the writer and returns once it is durable.
/// A caller never holds the registry lock across a call, because the writer reads the registry too.
pub(crate) trait DeviceSink: Send + Sync {
    /// Make one change durable. False when it could not be.
    fn record(&self, change: FloorDeviceChange) -> bool;
    /// Tell every open page about a change that is already durable.
    fn announce(&self, event: &DeviceEvent);
}

/// A change every open page is told about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeviceEvent {
    /// A browser signed in.
    Enrolled {
        device: DeviceId,
        name: String,
        at: Timestamp,
    },
    /// A new device was kept.
    Acknowledged { device: DeviceId },
    /// A device ended.
    Removed {
        device: DeviceId,
        reason: FloorDeviceRemoval,
    },
}

/// One signed-in browser as the pages and the command line list it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeviceView {
    pub(crate) device: DeviceId,
    pub(crate) name: String,
    pub(crate) signed_in: Timestamp,
    pub(crate) first_used: Option<Timestamp>,
    pub(crate) last_used: Option<Timestamp>,
    /// Whether nobody has answered its "new browser" notice yet.
    pub(crate) new: bool,
    /// Whether this is the browser that asked.
    pub(crate) this: bool,
}

/// Why a device is gone, as the page that holds its cookie is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GoneReason {
    Removed,
    Expired,
    Replaced,
}

impl GoneReason {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::Removed => "removed",
            Self::Expired => "expired",
            Self::Replaced => "replaced",
        }
    }

    const fn of(removal: FloorDeviceRemoval) -> Option<Self> {
        match removal {
            FloorDeviceRemoval::SignedOut => None,
            FloorDeviceRemoval::Removed => Some(Self::Removed),
            FloorDeviceRemoval::Expired | FloorDeviceRemoval::NeverUsed => Some(Self::Expired),
            FloorDeviceRemoval::Replaced => Some(Self::Replaced),
        }
    }
}

/// The result of asking to end a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Removal {
    Removed,
    Unknown,
    /// The journal could not take the removal, so the device is still signed in.
    Unavailable,
}

/// The result of keeping a new device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Keep {
    Kept,
    Unknown,
    /// A device cannot vouch for itself.
    Own,
    Unavailable,
}

/// The one response that crosses from the session module to the HTTP writer.
pub(crate) struct Exchange {
    pub(crate) cookie: String,
    /// The device the exchange enrolled.
    pub(crate) device: DeviceId,
    /// How long the browser keeps the cookie, in seconds.
    pub(crate) cookie_max_age_s: i64,
}

/// The body of a link exchange.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Enrolment {
    link: String,
    key: String,
    #[serde(default)]
    name: String,
}

impl fmt::Debug for Exchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Exchange")
            .field("cookie", &"<redacted>")
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

/// Why an exchange enrolled nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExchangeRefusal {
    /// The body is not an enrolment with a valid key.
    Malformed,
    /// The link is not good, the origin is not this listener's, the run is over, or the journal
    /// could not take the enrolment.
    Forbidden,
    /// The key in the body did not sign the request, so the link is left unspent.
    Signature,
}

/// An unforgeable admission to the future read-only projection facade.
pub struct ReadOnlySession {
    _private: (),
}

/// One confirmed reader, retained without extending its idle deadline.
pub struct ReceiptSession {
    identity: SessionIdentity,
    device: Hex32,
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
                    && session.id == reader.device
                    && session.live_at(now)
            })
    }

    /// Revoke one presented session under the registry lock; test-only caller access.
    #[doc(hidden)]
    pub fn revoke_for_test(&self, cookie: &str) -> bool {
        let cookie = digest(cookie.as_bytes());
        self.0
            .registry
            .lock()
            .end(&cookie, FloorDeviceRemoval::Removed)
            .is_some()
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

    /// Admit one exact-origin signed read and advance only its idle deadline.
    pub fn authorize_read(&self, request: &Presentation<'_>) -> Result<ReadOnlySession, Refusal> {
        self.0
            .admit(request, IdleClock::Advance, |_| Some(()))
            .map(|_| ReadOnlySession { _private: () })
    }

    /// Authenticate one browser upgrade and atomically acquire its per-session socket slot.
    pub(crate) fn authorize_stream(
        &self,
        request: &Presentation<'_>,
    ) -> Result<StreamSession, Refusal> {
        let ((), cookie, revoked) = self.admit_socket(request, |session| {
            (session.websockets < MAX_WEBSOCKETS_PER_SESSION).then(|| session.websockets += 1)
        })?;
        Ok(StreamSession {
            inner: Arc::clone(&self.0),
            cookie,
            revoked,
            visible: AtomicBool::new(false),
        })
    }

    pub(crate) fn authorize_terminal(
        &self,
        request: &Presentation<'_>,
    ) -> Result<TerminalSession, Refusal> {
        let (browser_epoch, cookie, revoked) = self.admit_socket(request, |session| {
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
        Ok(TerminalSession {
            inner: Arc::clone(&self.0),
            cookie,
            revoked,
            browser_epoch,
        })
    }

    /// Authenticate one engine sign-in upgrade and claim its session's one sign-in slot.
    pub(crate) fn authorize_login(
        &self,
        request: &Presentation<'_>,
    ) -> Result<LoginSession, Refusal> {
        let ((), cookie, revoked) = self.admit_socket(request, |session| {
            (!session.login).then(|| session.login = true)
        })?;
        Ok(LoginSession {
            inner: Arc::clone(&self.0),
            cookie,
            revoked,
        })
    }

    /// Authenticate one upgrade and let `take` claim its session's slot under the registry lock.
    /// Only a claimed slot advances the session's idle time.
    fn admit_socket<T>(
        &self,
        request: &Presentation<'_>,
        take: impl FnOnce(&mut Session) -> Option<T>,
    ) -> Result<(T, [u8; 32], Arc<AtomicBool>), Refusal> {
        let admitted = self.0.admit(request, IdleClock::Advance, take)?;
        Ok((admitted.value, admitted.cookie, admitted.revoked))
    }
}

/// A request as the registry checks it: where it says it came from, the cookie it carried, the
/// method, target and body its signature covers, and what its page presented. Nothing in it is
/// believed yet.
pub struct Presentation<'a> {
    host: &'a str,
    origin: &'a str,
    cookie: &'a str,
    method: &'a str,
    target: &'a str,
    body: &'a [u8],
    signed: Option<Presented>,
}

impl<'a> Presentation<'a> {
    /// A request with the `Host`, `Origin` and cookie it carried, and no signature yet.
    #[must_use]
    pub const fn new(
        host: &'a str,
        origin: &'a str,
        cookie: &'a str,
        method: &'a str,
        target: &'a str,
        body: &'a [u8],
    ) -> Self {
        Self {
            host,
            origin,
            cookie,
            method,
            target,
            body,
            signed: None,
        }
    }

    /// Add the four values a page presents: the origin it signed for, the time, the request id
    /// and the signature. Values that are malformed leave the request unsigned.
    #[must_use]
    pub fn signed(mut self, origin: &str, time: &str, request: &str, signature: &str) -> Self {
        self.signed = Presented::parse(origin, time, request, signature);
        self
    }

    fn subject(&self) -> Subject<'a> {
        Subject {
            method: self.method,
            target: self.target,
            body: self.body,
        }
    }
}

/// Why a request earned no device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The cookie names no live device, or the run is over, or Host and Origin are not this
    /// listener's.
    Gone,
    /// The cookie names a live device and the request did not prove it came from that device's
    /// key: unsigned, signed for another origin, outside the clock window, a reused id, a full
    /// memory of ids, or a signature that does not verify.
    Signature,
    /// The request was good and the socket slot it asked for is taken.
    Busy,
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
    /// What the page last said about being on screen. A page that has said nothing is not watched.
    visible: AtomicBool,
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
    Model,
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

    /// Sign this device out. The removal is durable before the device leaves the table, and its
    /// sockets close at their next check.
    pub(crate) fn sign_out(&self) -> Removal {
        self.inner
            .remove(self.cookie, FloorDeviceRemoval::SignedOut)
    }

    /// The device this session is, while it is in the table.
    pub(crate) fn device(&self) -> Option<Hex32> {
        self.inner
            .registry
            .lock()
            .sessions
            .get(&self.cookie)
            .map(|session| session.id)
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

    /// Record whether the page is on screen.
    pub(crate) fn set_visible(&self, visible: bool) {
        self.visible.store(visible, Ordering::Release);
    }

    /// Revalidate, and while the page is on screen renew the idle time at most once a minute. A
    /// device past its 90-day limit is gone from the table, so renewal never extends it.
    #[must_use]
    pub(crate) fn renew_if_watched(&self) -> bool {
        if !self.inner.valid.load(Ordering::Acquire) {
            return false;
        }
        let (live, used) = {
            let registry::Locked { mut state, now } = self.inner.registry.lock_at_now();
            state.prune(now);
            if !self.inner.valid.load(Ordering::Acquire) {
                return false;
            }
            match state.sessions.get_mut(&self.cookie) {
                None => (false, None),
                Some(session) => {
                    let used = (self.visible.load(Ordering::Acquire)
                        && now.millis_since(session.last_used) >= WATCH_RENEWAL_MS)
                        .then(|| session.note_use(now))
                        .flatten();
                    (true, used)
                }
            }
        };
        // The socket loop asks every few milliseconds, so it settles only when a use is due.
        if used.is_some() {
            self.inner.settle(used);
        }
        live
    }

    /// A successfully decoded client `floor.subscribe` is admitted activity.
    pub(crate) fn touch_subscribe(&self) -> bool {
        if !self.inner.valid.load(Ordering::Acquire) {
            return false;
        }
        let (live, used) = {
            let registry::Locked { mut state, now } = self.inner.registry.lock_at_now();
            state.prune(now);
            if !self.inner.valid.load(Ordering::Acquire) {
                return false;
            }
            match state.sessions.get_mut(&self.cookie) {
                None => (false, None),
                Some(session) => (true, session.note_use(now)),
            }
        };
        self.inner.settle(used);
        live
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
    /// Where device changes become durable and reach the open pages. Set by the daemon.
    sink: RwLock<Option<Arc<dyn DeviceSink>>>,
}

/// A request that earned its device, and what it claimed.
struct Checked<T> {
    value: T,
    cookie: [u8; 32],
    revoked: Arc<AtomicBool>,
}

impl Inner {
    fn authority_matches(&self, host: &str, origin: &str) -> bool {
        host == self.host && origin == self.origin
    }

    /// Check a request against its device and let `take` claim what it asked for under the
    /// registry lock.
    ///
    /// The cookie must name a live device, or the answer is [`Refusal::Gone`] and nothing about
    /// the request is read. Then the page's origin must be this listener's, the request's own
    /// and the device's; its time within a minute of the clock; and its signature must verify
    /// under the device's key. Only a request that verified spends one of the device's request
    /// ids, so a forged request cannot use them up. `idle` says whether a good request counts as
    /// the device's use.
    fn admit<T>(
        &self,
        request: &Presentation<'_>,
        idle: IdleClock,
        take: impl FnOnce(&mut Session) -> Option<T>,
    ) -> Result<Checked<T>, Refusal> {
        if !self.authority_matches(request.host, request.origin) {
            return Err(Refusal::Gone);
        }
        let cookie = digest(request.cookie.as_bytes());
        let (device, key, device_origin, revoked, now) = {
            let registry::Locked { mut state, now } = self.registry.lock_at_now();
            if !self.valid.load(Ordering::Acquire) {
                return Err(Refusal::Gone);
            }
            if idle == IdleClock::Advance {
                state.prune(now);
            }
            let found = state
                .sessions
                .get(&cookie)
                .filter(|session| session.live_at(now) && !session.revoked.load(Ordering::Acquire))
                .map(|session| {
                    (
                        session.id,
                        session.key.clone(),
                        session.origin.clone(),
                        Arc::clone(&session.revoked),
                        now,
                    )
                });
            let Some(found) = found else {
                drop(state);
                self.settle(None);
                return Err(Refusal::Gone);
            };
            found
        };
        let presented = request.signed.as_ref().ok_or(Refusal::Signature)?;
        if presented.origin != request.origin
            || presented.origin != self.origin
            || presented.origin != device_origin
            || now.unix_millis().abs_diff(presented.millis) > SIGNATURE_WINDOW_MS.unsigned_abs()
            || !key.verifies(device, &request.subject(), presented)
        {
            return Err(Refusal::Signature);
        }
        let (value, used) = {
            let registry::Locked { mut state, now } = self.registry.lock_at_now();
            if !self.valid.load(Ordering::Acquire) {
                return Err(Refusal::Gone);
            }
            let Some(session) = state.sessions.get_mut(&cookie).filter(|session| {
                Arc::ptr_eq(&session.revoked, &revoked)
                    && session.live_at(now)
                    && !session.revoked.load(Ordering::Acquire)
            }) else {
                return Err(Refusal::Gone);
            };
            if !session.requests.admit(now, presented.request) {
                return Err(Refusal::Signature);
            }
            let Some(value) = take(session) else {
                return Err(Refusal::Busy);
            };
            let used = (idle == IdleClock::Advance)
                .then(|| session.note_use(now))
                .flatten();
            (value, used)
        };
        self.settle(used);
        Ok(Checked {
            value,
            cookie,
            revoked,
        })
    }

    fn sink(&self) -> Option<Arc<dyn DeviceSink>> {
        self.sink
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// A fresh cookie that no live session holds.
    fn unused_cookie(&self) -> String {
        let state = self.registry.lock();
        loop {
            let candidate = mint_secret();
            if !state.sessions.contains_key(&digest(candidate.as_bytes())) {
                return candidate;
            }
        }
    }

    /// Make one change durable. Without a sink there is nothing to make durable.
    fn record(&self, change: FloorDeviceChange) -> bool {
        self.sink().is_none_or(|sink| sink.record(change))
    }

    fn announce(&self, event: &DeviceEvent) {
        if let Some(sink) = self.sink() {
            sink.announce(event);
        }
    }

    /// Journal what a request just did to its device, then journal the devices that ran out since
    /// the last call. Called with no lock held, and only from a thread that is not the writer.
    fn settle(&self, used: Option<Used>) {
        if let Some(used) = used {
            let change = match used.kind {
                UseKind::First => FloorDeviceChange::FirstUse(used.device),
                UseKind::Daily => FloorDeviceChange::Use(used.device),
            };
            if !self.record(change) {
                tracing::warn!("a Floor device's use could not be recorded");
                if let Some(session) = self
                    .registry
                    .lock()
                    .sessions
                    .values_mut()
                    .find(|session| session.id == used.device)
                {
                    if used.kind == UseKind::First {
                        session.first_used = None;
                    }
                    session.journaled_use = used.journaled_before;
                }
            }
        }
        let ended = std::mem::take(&mut self.registry.lock().ended);
        for (device, reason) in ended {
            if self.record(FloorDeviceChange::Remove { device, reason }) {
                self.announce(&DeviceEvent::Removed {
                    device: device.into(),
                    reason,
                });
            }
        }
    }

    /// End one device: its removal is durable first, then it leaves the table, so a removal that
    /// could not be recorded leaves the device signed in.
    fn remove(&self, key: [u8; 32], reason: FloorDeviceRemoval) -> Removal {
        let Some(device) = self
            .registry
            .lock()
            .sessions
            .get(&key)
            .map(|session| session.id)
        else {
            return Removal::Unknown;
        };
        if !self.record(FloorDeviceChange::Remove { device, reason }) {
            return Removal::Unavailable;
        }
        if self.registry.lock().end(&key, reason).is_none() {
            // Expiry ended it between the record and here. Its removal is journaled, so the
            // expiry's own record would only repeat it.
            self.registry
                .lock()
                .ended
                .retain(|(ended, _)| *ended != device);
        }
        self.announce(&DeviceEvent::Removed {
            device: device.into(),
            reason,
        });
        Removal::Removed
    }
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

/// A sign-in link waiting to be exchanged.
struct Link {
    issued: Timestamp,
    purpose: LinkPurpose,
}

impl Link {
    const fn lifetime_ms(&self) -> i64 {
        match self.purpose {
            LinkPurpose::Open => OPEN_LINK_MS,
            LinkPurpose::Print => PRINTED_LINK_MS,
        }
    }
}

/// An ended device the daemon can still name, so a page holding its cookie hears why.
struct Ended {
    cookie: [u8; 32],
    reason: GoneReason,
}

#[derive(Default)]
struct State {
    imported: bool,
    pending: BTreeMap<[u8; 32], Link>,
    sessions: BTreeMap<[u8; 32], Session>,
    inflight: BTreeMap<(ActionKind, Hex32), Hex32>,
    /// The most recent ended devices, oldest first.
    remembered: std::collections::VecDeque<Ended>,
    /// Devices that ran out and whose removal is not journaled yet.
    ended: Vec<(Hex32, FloorDeviceRemoval)>,
}

/// One signed-in browser.
struct Session {
    id: Hex32,
    name: String,
    revoked: Arc<AtomicBool>,
    /// The key that checks every request this device makes.
    key: DeviceKey,
    /// The Floor origin it signed in on. A request signed for another origin is refused.
    origin: String,
    /// The request ids this device used lately, so none works twice.
    requests: Requests,
    created: Timestamp,
    first_used: Option<Timestamp>,
    last_used: Timestamp,
    /// The last use the journal holds, so a device is journaled at most once a day.
    journaled_use: Timestamp,
    /// Whether someone answered this device's "new browser" notice.
    acknowledged: bool,
    websockets: usize,
    terminal: Option<TerminalSlot>,
    /// Whether this session holds its one engine sign-in socket.
    login: bool,
}

/// The ids of a device's recent requests. An id is refused while it is remembered, and nothing is
/// admitted while the memory is full, so a flood of valid requests cannot grow it.
#[derive(Default)]
struct Requests {
    seen: Vec<(Timestamp, [u8; 16])>,
}

impl Requests {
    /// Remember `id` and say so, or say that it was used lately or that no room is left.
    fn admit(&mut self, now: Timestamp, id: [u8; 16]) -> bool {
        self.seen
            .retain(|(at, _)| now.millis_since(*at) < REQUEST_MEMORY_MS);
        if self.seen.len() >= MAX_REQUEST_IDS || self.seen.iter().any(|(_, seen)| *seen == id) {
            return false;
        }
        self.seen.push((now, id));
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UseKind {
    First,
    Daily,
}

/// A use the journal has to hear about once the lock is released.
struct Used {
    device: Hex32,
    kind: UseKind,
    /// The journaled-use time before this request, restored if the journal refuses the record so
    /// the next request records it again.
    journaled_before: Timestamp,
}

impl Session {
    fn live_at(&self, now: Timestamp) -> bool {
        now.millis_since(self.created) < ABSOLUTE_EXPIRES_MS
            && now.millis_since(self.last_used) < IDLE_EXPIRES_MS
            && (self.first_used.is_some() || now.millis_since(self.created) < NEVER_USED_EXPIRES_MS)
    }

    /// Why a device that is not live at `now` ran out.
    fn expiry(&self, now: Timestamp) -> FloorDeviceRemoval {
        if self.first_used.is_none() && now.millis_since(self.created) >= NEVER_USED_EXPIRES_MS {
            FloorDeviceRemoval::NeverUsed
        } else {
            FloorDeviceRemoval::Expired
        }
    }

    /// How long the browser should keep this session's cookie: the soonest of the limits that
    /// apply, in whole seconds and never below one.
    fn cookie_max_age_s(&self, now: Timestamp) -> i64 {
        let idle = IDLE_EXPIRES_MS - now.millis_since(self.last_used);
        let absolute = ABSOLUTE_EXPIRES_MS - now.millis_since(self.created);
        let unused = if self.first_used.is_none() {
            NEVER_USED_EXPIRES_MS - now.millis_since(self.created)
        } else {
            i64::MAX
        };
        (idle.min(absolute).min(unused) / 1000).max(1)
    }

    /// Count a request as use. The first request, and the first one a day after the journal last
    /// heard of this device, must be journaled.
    fn note_use(&mut self, now: Timestamp) -> Option<Used> {
        self.last_used = now;
        let journaled_before = self.journaled_use;
        let kind = if self.first_used.is_none() {
            self.first_used = Some(now);
            UseKind::First
        } else if now.millis_since(self.journaled_use) >= USE_JOURNAL_MS {
            UseKind::Daily
        } else {
            return None;
        };
        self.journaled_use = now;
        Some(Used {
            device: self.id,
            kind,
            journaled_before,
        })
    }

    /// The device as the table lists it.
    fn view(&self, key: &[u8; 32], me: Option<&[u8; 32]>) -> DeviceView {
        DeviceView {
            device: self.id.into(),
            name: self.name.clone(),
            signed_in: self.created,
            first_used: self.first_used,
            last_used: self.first_used.map(|_| self.last_used),
            new: !self.acknowledged,
            this: me == Some(key),
        }
    }
}

impl State {
    fn prune(&mut self, now: Timestamp) {
        self.pending
            .retain(|_, link| now.millis_since(link.issued) < link.lifetime_ms());
        let dead: Vec<[u8; 32]> = self
            .sessions
            .iter()
            .filter(|(_, session)| !session.live_at(now))
            .map(|(key, _)| *key)
            .collect();
        for key in dead {
            if let Some(session) = self.sessions.remove(&key) {
                let reason = session.expiry(now);
                self.remember(&key, reason);
                self.ended.push((session.id, reason));
            }
        }
    }

    /// How many links for `purpose` still wait to be exchanged.
    fn waiting(&self, purpose: LinkPurpose) -> usize {
        self.pending
            .values()
            .filter(|link| link.purpose == purpose)
            .count()
    }

    fn remember(&mut self, key: &[u8; 32], reason: FloorDeviceRemoval) {
        if let Some(reason) = GoneReason::of(reason) {
            self.remember_cookie(*key, reason);
        }
    }

    fn remember_cookie(&mut self, cookie: [u8; 32], reason: GoneReason) {
        if self.remembered.len() >= MAX_REMEMBERED_ENDED {
            self.remembered.pop_front();
        }
        self.remembered.push_back(Ended { cookie, reason });
    }

    /// End one session: it leaves the table, and everything holding its revoked flag sees it.
    /// Returns its device.
    fn end(&mut self, key: &[u8; 32], reason: FloorDeviceRemoval) -> Option<Hex32> {
        let session = self.sessions.remove(key)?;
        session.revoked.store(true, Ordering::Release);
        self.remember(key, reason);
        Some(session.id)
    }

    /// The sessions to end so that at most [`MAX_DEVICES`] remain, never `keep`. A device that
    /// never made a request goes first, then one nobody has answered the notice for, then the one
    /// used longest ago. Equal uses go to the older sign-in, then to the smaller key, so the
    /// choice never depends on map order.
    fn surplus(&self, keep: Option<&[u8; 32]>) -> Vec<[u8; 32]> {
        let excess = self.sessions.len().saturating_sub(MAX_DEVICES);
        let mut candidates: Vec<_> = self
            .sessions
            .iter()
            .filter(|(key, _)| Some(*key) != keep)
            .map(|(key, session)| {
                let tier = if session.first_used.is_none() {
                    0_u8
                } else if !session.acknowledged {
                    1
                } else {
                    2
                };
                (
                    tier,
                    session.last_used.unix_millis(),
                    session.created.unix_millis(),
                    *key,
                )
            })
            .collect();
        candidates.sort_unstable();
        candidates
            .into_iter()
            .take(excess)
            .map(|(_, _, _, key)| key)
            .collect()
    }
}

/// Strip control characters, trim, cap at 64 characters, and name an empty result "Browser".
fn clean_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() && !is_format_char(*c))
        .collect();
    let capped: String = cleaned.trim().chars().take(MAX_NAME_CHARS).collect();
    let capped = capped.trim_end().to_owned();
    if capped.is_empty() {
        DEFAULT_NAME.to_owned()
    } else {
        capped
    }
}

/// Unicode format characters (category Cf) draw nothing but can reorder or hide the text around
/// them, so a name never keeps them.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{AD}'
            | '\u{600}'..='\u{605}'
            | '\u{61C}'
            | '\u{6DD}'
            | '\u{70F}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

impl FloorSessions {
    #[cfg(test)]
    pub(crate) fn registry_counts_for_test(&self) -> (usize, usize) {
        let state = self.0.registry.lock();
        (state.pending.len(), state.sessions.len())
    }

    /// The rows the journal would hold for the devices signed in now, so a test can start a run
    /// that finds them.
    #[cfg(test)]
    pub(crate) fn rows_for_test(&self) -> BTreeMap<Hex32, DeviceRow> {
        let origin = self.0.origin.clone();
        self.0
            .registry
            .lock()
            .sessions
            .iter()
            .map(|(cookie, session)| {
                (
                    session.id,
                    DeviceRow {
                        origin: origin.clone(),
                        name: session.name.clone(),
                        cookie: Hex32::new(*cookie),
                        key: Some(session.key.text()),
                        enrolled: session.created,
                        first_used: session.first_used,
                        last_used: Some(session.last_used),
                        acknowledged: None,
                        removed: None,
                    },
                )
            })
            .collect()
    }

    /// Admit one exact-origin signed browser action — onboarding, an answer, a pause or a
    /// resume — and retain only its session digest.
    pub(crate) fn authorize_onboard(
        &self,
        request: &Presentation<'_>,
    ) -> Result<SessionIdentity, Refusal> {
        self.authenticate(request, IdleClock::Advance)
            .map(|(identity, _)| identity)
    }

    /// Admit a request that asks about the session itself, and leave its idle deadline alone.
    pub(crate) fn authorize_standing(
        &self,
        request: &Presentation<'_>,
    ) -> Result<SessionIdentity, Refusal> {
        self.authenticate(request, IdleClock::Hold)
            .map(|(identity, _)| identity)
    }

    pub(crate) fn authorize_receipt(
        &self,
        request: &Presentation<'_>,
    ) -> Result<ReceiptSession, Refusal> {
        self.authenticate(request, IdleClock::Hold)
            .map(|(identity, device)| ReceiptSession { identity, device })
    }

    fn authenticate(
        &self,
        request: &Presentation<'_>,
        idle: IdleClock,
    ) -> Result<(SessionIdentity, Hex32), Refusal> {
        let admitted = self.0.admit(request, idle, |session| Some(session.id))?;
        Ok((
            SessionIdentity {
                inner: Arc::clone(&self.0),
                cookie: admitted.cookie,
                revoked: admitted.revoked,
            },
            admitted.value,
        ))
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
            sink: RwLock::new(None),
        }))
    }

    /// Give the device table somewhere to be made durable.
    pub(crate) fn install_sink(&self, sink: Arc<dyn DeviceSink>) {
        *self.0.sink.write().unwrap_or_else(PoisonError::into_inner) = Some(sink);
    }

    /// Let a test that drives the daemon's loop by hand sign browsers in without a running loop to
    /// make them durable.
    #[cfg(test)]
    pub(crate) fn detach_devices_for_test(&self) {
        *self.0.sink.write().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Seed the table with the devices the journal holds. A device that has run out ends, and so
    /// does any beyond the limit, and their removals are journaled by the first request or listing
    /// that follows. A device signed in on another origin cannot be used here, so it stays out of
    /// the table and out of the count, and is journaled as nothing: it returns if the daemon does.
    ///
    /// A browser that signed in before requests were signed holds no key and can never act again.
    /// It ends once, as expired, whatever origin it signed in on, so its page hears that its
    /// sign-in expired and the next boot finds nothing left to end.
    pub(crate) fn seed(&self, rows: &BTreeMap<Hex32, DeviceRow>) {
        let registry::Locked { mut state, now } = self.0.registry.lock_at_now();
        let mut ended: Vec<_> = rows
            .iter()
            .filter_map(|(device, row)| row.removed.map(|(reason, at)| (*device, row, reason, at)))
            .collect();
        ended.sort_by_key(|(_, _, _, at)| *at);
        for (_, row, reason, at) in ended {
            if let Some(reason) = GoneReason::of(reason)
                && now.millis_since(at) < ABSOLUTE_EXPIRES_MS
            {
                state.remember_cookie(row.cookie.bytes(), reason);
            }
        }
        for (device, row) in rows.iter().filter(|(_, row)| row.removed.is_none()) {
            let Some((key, id)) = row.key.as_deref().and_then(DeviceKey::parse) else {
                state.remember_cookie(row.cookie.bytes(), GoneReason::Expired);
                state.ended.push((*device, FloorDeviceRemoval::Expired));
                continue;
            };
            if id != *device || row.origin != self.0.origin {
                continue;
            }
            let last_used = row.last_used.unwrap_or(row.enrolled);
            state.sessions.insert(
                row.cookie.bytes(),
                Session {
                    id: *device,
                    name: row.name.clone(),
                    revoked: Arc::new(AtomicBool::new(false)),
                    key,
                    origin: row.origin.clone(),
                    requests: Requests::default(),
                    created: row.enrolled,
                    first_used: row.first_used,
                    last_used,
                    journaled_use: last_used,
                    acknowledged: row.acknowledged.is_some(),
                    websockets: 0,
                    terminal: None,
                    login: false,
                },
            );
        }
        state.prune(now);
        for victim in state.surplus(None) {
            if let Some(device) = state.end(&victim, FloorDeviceRemoval::Replaced) {
                state.ended.push((device, FloorDeviceRemoval::Replaced));
            }
        }
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

    /// Mint one link to open, which lives 60 seconds, retaining only its digest.
    pub fn issue(&self) -> Result<SessionIssueResult, IssueError> {
        self.issue_for(LinkPurpose::Open)
    }

    /// Mint one link, retaining only its digest. A link to open lives 60 seconds and is refused
    /// while four wait. A link to print lives 10 minutes in a table of its own, where a new one
    /// replaces the oldest, so printing never takes a slot `dudes open` needs.
    pub fn issue_for(&self, purpose: LinkPurpose) -> Result<SessionIssueResult, IssueError> {
        if !self.0.valid.load(Ordering::Acquire) {
            return Err(IssueError::RunEnded);
        }
        let registry::Locked { mut state, now } = self.0.registry.lock_at_now();
        if !self.0.valid.load(Ordering::Acquire) {
            return Err(IssueError::RunEnded);
        }
        state.prune(now);
        match purpose {
            LinkPurpose::Open if state.waiting(purpose) >= MAX_PENDING_OPEN_LINKS => {
                return Err(IssueError::Capacity);
            }
            LinkPurpose::Print => {
                while state.waiting(purpose) >= MAX_PENDING_PRINTED_LINKS {
                    let oldest = state
                        .pending
                        .iter()
                        .filter(|(_, link)| link.purpose == purpose)
                        .min_by_key(|(key, link)| (link.issued.unix_millis(), **key))
                        .map(|(key, _)| *key);
                    let Some(oldest) = oldest else { break };
                    state.pending.remove(&oldest);
                }
            }
            LinkPurpose::Open => {}
        }
        let bootstrap = loop {
            let candidate = mint_secret();
            if !state.pending.contains_key(&digest(candidate.as_bytes())) {
                break candidate;
            }
        };
        state.pending.insert(
            digest(bootstrap.as_bytes()),
            Link {
                issued: now,
                purpose,
            },
        );
        Ok(SessionIssueResult {
            base_url: self.0.origin.clone(),
            bootstrap,
            expires_in_s: match purpose {
                LinkPurpose::Open => SESSION_EXPIRES_IN_S,
                LinkPurpose::Print => SESSION_PRINTED_EXPIRES_IN_S,
            },
        })
    }

    /// Exchange one valid link under this listener's exact Host and Origin. The body is JSON: the
    /// link, the browser's public key as SPKI DER in unpadded base64url, and the name the browser
    /// gives itself.
    ///
    /// The browser becomes a device only once its enrolment is durable, and the cookie leaves only
    /// after that. A body without a valid key is refused before the link is touched. A link is
    /// spent by an attempt that gets past that, so a journal that cannot take the enrolment costs
    /// the person one more `dudes open`. A key that is already a live device is replaced: its old
    /// cookie stops working and the browser holds the new one.
    pub(crate) fn exchange(&self, request: &Presentation<'_>) -> Result<Exchange, ExchangeRefusal> {
        use ExchangeRefusal::{Forbidden, Malformed, Signature};
        let (host, origin, body) = (request.host, request.origin, request.body);
        if !self.0.authority_matches(host, origin) || !self.0.valid.load(Ordering::Acquire) {
            return Err(Forbidden);
        }
        let Enrolment {
            link,
            key: key_text,
            name,
        } = serde_json::from_slice(body).map_err(|_| Malformed)?;
        let (device_key, device) = DeviceKey::parse(&key_text).ok_or(Malformed)?;
        // The key proves itself before the link is looked at, so someone who holds a live link and
        // knows a public key cannot enrol a key whose private half they lack.
        let presented = request.signed.as_ref().ok_or(Signature)?;
        let now = self.0.registry.lock_at_now().now;
        if presented.origin != request.origin
            || now.unix_millis().abs_diff(presented.millis) > SIGNATURE_WINDOW_MS.unsigned_abs()
            || !device_key.verifies(device, &request.subject(), presented)
        {
            return Err(Signature);
        }
        let name = clean_name(&name);
        let link = digest(link.as_bytes());
        let replaced = {
            let registry::Locked { mut state, now } = self.0.registry.lock_at_now();
            if !self.0.valid.load(Ordering::Acquire) {
                return Err(Forbidden);
            }
            state.prune(now);
            // Pruning has dropped every expired link, so a link found here is good, and nothing
            // after this refuses for want of room: a full table makes room instead.
            if state.pending.remove(&link).is_none() {
                drop(state);
                self.0.settle(None);
                return Err(Forbidden);
            }
            state
                .sessions
                .iter()
                .find(|(_, session)| session.id == device)
                .map(|(cookie, _)| *cookie)
        };
        if let Some(cookie) = replaced
            && self.0.remove(cookie, FloorDeviceRemoval::Replaced) == Removal::Unavailable
        {
            return Err(Forbidden);
        }
        let cookie = self.0.unused_cookie();
        let key = digest(cookie.as_bytes());
        if !self.0.record(FloorDeviceChange::Enroll {
            device,
            origin: self.0.origin.clone(),
            name: name.clone(),
            key: key_text,
            cookie: Hex32::new(key),
        }) {
            return Err(Forbidden);
        }
        let (max_age, surplus, at) = {
            let registry::Locked { mut state, now } = self.0.registry.lock_at_now();
            if !self.0.valid.load(Ordering::Acquire) {
                return Err(Forbidden);
            }
            let session = Session {
                id: device,
                name: name.clone(),
                revoked: Arc::new(AtomicBool::new(false)),
                key: device_key,
                origin: self.0.origin.clone(),
                requests: {
                    let mut requests = Requests::default();
                    requests.admit(now, presented.request);
                    requests
                },
                created: now,
                first_used: None,
                last_used: now,
                journaled_use: now,
                acknowledged: false,
                websockets: 0,
                terminal: None,
                login: false,
            };
            let max_age = session.cookie_max_age_s(now);
            state.sessions.insert(key, session);
            (max_age, state.surplus(Some(&key)), now)
        };
        self.0.announce(&DeviceEvent::Enrolled {
            device: device.into(),
            name,
            at,
        });
        for victim in surplus {
            self.0.remove(victim, FloorDeviceRemoval::Replaced);
        }
        self.0.settle(None);
        Ok(Exchange {
            cookie,
            device: device.into(),
            cookie_max_age_s: max_age,
        })
    }

    /// Whether this run still honours sessions. Once it is stopping, an unknown session is not
    /// news about the session, because every session was just cleared.
    pub(crate) fn is_running(&self) -> bool {
        self.0.valid.load(Ordering::Acquire)
    }

    /// Why the device behind this cookie is gone, when it ended for a reason the person should
    /// hear. The reason is not authority: it opens nothing, so the cookie alone earns it.
    pub(crate) fn gone_reason(&self, cookie: &str) -> Option<GoneReason> {
        let cookie = digest(cookie.as_bytes());
        let state = self.0.registry.lock();
        state
            .remembered
            .iter()
            .rev()
            .find(|ended| ended.cookie == cookie)
            .map(|ended| ended.reason)
    }

    /// Whether the cookie names a device that is live now. With `sweep`, a device that ran out is
    /// ended and remembered here, so the refusal that follows can say why. A request that only
    /// reads leaves the registry as it found it.
    pub(crate) fn knows(&self, cookie: &str, sweep: bool) -> bool {
        let cookie = digest(cookie.as_bytes());
        let live = {
            let registry::Locked { mut state, now } = self.0.registry.lock_at_now();
            if sweep {
                state.prune(now);
            }
            self.0.valid.load(Ordering::Acquire)
                && state.sessions.get(&cookie).is_some_and(|session| {
                    session.live_at(now) && !session.revoked.load(Ordering::Acquire)
                })
        };
        if sweep {
            self.0.settle(None);
        }
        live
    }

    /// The signed-in devices, newest sign-in first. `me` marks the asking browser.
    pub(crate) fn devices(&self, me: Option<&SessionIdentity>) -> Vec<DeviceView> {
        let me = me.map(|identity| identity.cookie);
        let views = {
            let registry::Locked { mut state, now } = self.0.registry.lock_at_now();
            state.prune(now);
            let mut views: Vec<_> = state
                .sessions
                .iter()
                .map(|(key, session)| session.view(key, me.as_ref()))
                .collect();
            views.sort_by(|a, b| {
                (b.signed_in.unix_millis(), b.device).cmp(&(a.signed_in.unix_millis(), a.device))
            });
            views
        };
        self.0.settle(None);
        views
    }

    /// Sign one device out by its id.
    pub(crate) fn remove_device(&self, device: DeviceId, reason: FloorDeviceRemoval) -> Removal {
        let device = Hex32::from(device);
        let key = self
            .0
            .registry
            .lock()
            .sessions
            .iter()
            .find(|(_, session)| session.id == device)
            .map(|(key, _)| *key);
        let outcome = key.map_or(Removal::Unknown, |key| self.0.remove(key, reason));
        self.0.settle(None);
        outcome
    }

    /// Keep a new device: someone looked at it and answered the notice. A device cannot keep
    /// itself, because whoever holds it already could.
    pub(crate) fn keep_device(&self, device: DeviceId, by: &SessionIdentity) -> Keep {
        self.keep(device, Some(by.cookie))
    }

    /// Keep a new device on the operator's word, which no browser has to give.
    pub(crate) fn keep_device_as_operator(&self, device: DeviceId) -> Keep {
        self.keep(device, None)
    }

    fn keep(&self, device: DeviceId, by: Option<[u8; 32]>) -> Keep {
        let device = Hex32::from(device);
        let found = {
            let state = self.0.registry.lock();
            state
                .sessions
                .iter()
                .find(|(_, session)| session.id == device)
                .map(|(key, session)| (*key, session.acknowledged))
        };
        let Some((key, acknowledged)) = found else {
            return Keep::Unknown;
        };
        if by == Some(key) {
            return Keep::Own;
        }
        if acknowledged {
            return Keep::Kept;
        }
        if !self.0.record(FloorDeviceChange::Acknowledge(device)) {
            return Keep::Unavailable;
        }
        if let Some(session) = self.0.registry.lock().sessions.get_mut(&key) {
            session.acknowledged = true;
        }
        self.0.announce(&DeviceEvent::Acknowledged {
            device: device.into(),
        });
        Keep::Kept
    }

    /// End this run's authority before its listener and connection owners are released. The
    /// devices stay in the journal: only this run's hold on them ends.
    pub(crate) fn invalidate(&self) {
        let mut state = self.0.registry.lock();
        self.0.valid.store(false, Ordering::Release);
        for session in state.sessions.values() {
            session.revoked.store(true, Ordering::Release);
        }
        state.pending.clear();
        state.sessions.clear();
        state.inflight.clear();
        state.ended.clear();
    }
}

/// The Floor's devices and the run-local counts that follow them, which `dudes upgrade` carries
/// across its restart through `floor.session.export` and `floor.session.import`.
///
/// The devices themselves come back from the journal, so the export carries only what the
/// journal does not hold: the counts. Lock order is the session registry, then a count's own
/// lock. No count's lock is held while the registry is taken.
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

    /// Every live device's counts as this run holds them. Nothing is refreshed, and no cookie,
    /// secret, link, terminal grant or reservation leaves: only digests and counts do.
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

    /// Take the counts an earlier run exported for the devices this run holds, and say how many
    /// devices that was. An entry for a digest this run does not hold is ignored, which is every
    /// entry an earlier release exported, because it carried sessions and not devices. A run
    /// takes one import, from a run other than itself, and any attempt that passes the run check
    /// uses it up, whatever else it is refused for.
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
            if !state.sessions.contains_key(&key) {
                continue;
            }
            if let Some(tickets) = &self.tickets {
                tickets.seed(key, entry.ticket_creates, entry.ticket_assigns);
            }
            self.checks.seed(entry.digest, entry.check_runs);
            restored += 1;
        }
        restored
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

    use super::test_support::Joined;
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
            .join(host, origin, &issued.bootstrap)
            .expect("session");
        let identity = sessions
            .enter(host, origin, &exchange)
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
            .join(host, origin, &first.bootstrap)
            .expect("first session");
        let identity = sessions
            .enter(host, origin, &first)
            .expect("first admitted identity");
        let attempt = Hex32::new([0; 32]);
        let digest = Hex32::new([1; 32]);
        let ReserveResult::Owner(_owner) = identity.reserve(ActionKind::Onboard, attempt, digest)
        else {
            panic!("initial attempt reservation");
        };
        let second = sessions.issue().expect("other bootstrap");
        let second = sessions
            .join(host, origin, &second.bootstrap)
            .expect("other session");
        let holder = sessions
            .enter(host, origin, &second)
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
            .join(host, origin, &first.bootstrap)
            .expect("first session");
        let second = sessions.issue().expect("other bootstrap");
        let second = sessions
            .join(host, origin, &second.bootstrap)
            .expect("other session");
        let holder = sessions
            .enter(host, origin, &second)
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
            waiting.enter(host, origin, &first).is_none()
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
            .join(host, origin, &first.bootstrap)
            .expect("first session");
        let identity = sessions
            .enter(host, origin, &first)
            .expect("first admitted identity");
        let second = sessions.issue().expect("other bootstrap");
        let second = sessions
            .join(host, origin, &second.bootstrap)
            .expect("other session");
        let holder = sessions
            .enter(host, origin, &second)
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

    fn probe_session(sessions: &FloorSessions) -> Joined {
        let issued = sessions.issue().expect("bootstrap");
        sessions
            .join(PROBE_HOST, PROBE_ORIGIN, &issued.bootstrap)
            .expect("live session")
    }

    /// Another live session, whose append permit will hold the registry lock.
    fn probe_holder(sessions: &FloorSessions) -> SessionIdentity {
        let other = probe_session(sessions);
        sessions
            .enter(PROBE_HOST, PROBE_ORIGIN, &other)
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
        let (exchanged, sampled_while_held) = behind_permit(&clock, held, OPEN_LINK_MS, || {
            sessions
                .join(PROBE_HOST, PROBE_ORIGIN, &pending.bootstrap)
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
        let request = reader.present(PROBE_HOST, PROBE_ORIGIN, IDLE_EXPIRES_MS);
        let (admitted, sampled_while_held) = behind_permit(&clock, held, IDLE_EXPIRES_MS, || {
            authorizer.authorize_read(&request).is_ok()
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
                .stream(PROBE_HOST, PROBE_ORIGIN, &browser)
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
            .stream(PROBE_HOST, PROBE_ORIGIN, &owner)
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
            .stream(PROBE_HOST, PROBE_ORIGIN, &owner)
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
    fn the_device_table_is_bounded_at_eight_and_the_open_link_table_at_four() {
        let clock = Arc::new(FakeClock::frozen(0));
        let sessions = FloorSessions::new(
            "127.0.0.1:43123".parse().expect("loopback address"),
            Arc::clone(&clock),
        );
        let host = "127.0.0.1:43123";
        let origin = "http://127.0.0.1:43123";

        for _ in 0..MAX_DEVICES {
            let issued = sessions.issue().expect("one bounded link");
            assert!(sessions.join(host, origin, &issued.bootstrap).is_some());
        }
        let ninth = sessions.issue().expect("a pending link still fits");
        assert!(
            sessions.join(host, origin, &ninth.bootstrap).is_some(),
            "a ninth sign-in makes room instead of being refused"
        );
        assert_eq!(sessions.registry_counts_for_test().1, MAX_DEVICES);

        for _ in 0..MAX_PENDING_OPEN_LINKS {
            sessions.issue().expect("one of four pending links");
        }
        assert_eq!(sessions.issue(), Err(IssueError::Capacity));
        clock.advance(OPEN_LINK_MS);
        assert!(sessions.issue().is_ok(), "expiry releases pending capacity");
    }

    const HOUR_MS: i64 = 60 * 60_000;

    /// Answer a new device's notice as another device would, so it ranks as an ordinary one.
    fn keep_for_test(sessions: &FloorSessions, session: &Joined) {
        assert!(acts(sessions, session), "a kept device has been used");
        let mut state = sessions.0.registry.lock();
        let device = state
            .sessions
            .get_mut(&digest(session.cookie.as_bytes()))
            .expect("signed in");
        device.acknowledged = true;
    }
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

    fn sign_in(sessions: &FloorSessions) -> Joined {
        let issued = sessions.issue().expect("a bootstrap");
        sessions
            .join(LIFETIME_HOST, LIFETIME_ORIGIN, &issued.bootstrap)
            .expect("a session")
    }

    fn acts(sessions: &FloorSessions, session: &Joined) -> bool {
        sessions
            .enter(LIFETIME_HOST, LIFETIME_ORIGIN, session)
            .is_some()
    }

    fn watch(sessions: &FloorSessions, session: &Joined, visible: bool) -> StreamSession {
        let stream = sessions
            .authorizer()
            .stream(LIFETIME_HOST, LIFETIME_ORIGIN, session)
            .expect("an open stream");
        stream.set_visible(visible);
        stream
    }

    #[test]
    fn a_visible_open_stream_keeps_its_session_and_a_hidden_one_does_not() {
        let (clock, sessions) = lifetime_sessions();
        let (shown, hidden) = (sign_in(&sessions), sign_in(&sessions));
        let (shown_stream, hidden_stream) = (
            watch(&sessions, &shown, true),
            watch(&sessions, &hidden, false),
        );
        for _ in 0..31 {
            clock.advance(DAY_MS);
            assert!(shown_stream.renew_if_watched());
            let _ = hidden_stream.renew_if_watched();
        }
        assert!(acts(&sessions, &shown), "31 days on screen, no requests");
        assert!(
            !acts(&sessions, &hidden),
            "a hidden tab ended after 30 days"
        );
        assert!(!hidden_stream.renew_if_watched());
        assert_eq!(shown_stream.renew_if_watched(), acts(&sessions, &shown));
    }

    #[test]
    fn watching_never_outlasts_ninety_days_and_renews_at_most_once_a_minute() {
        let (clock, sessions) = lifetime_sessions();
        let shown = sign_in(&sessions);
        let stream = watch(&sessions, &shown, true);
        clock.advance(59_000);
        assert!(stream.renew_if_watched());
        clock.advance(30 * DAY_MS + 30_000 - 59_000);
        assert!(
            !acts(&sessions, &shown),
            "a renewal inside a minute did not move the idle time"
        );
        let (clock, sessions) = lifetime_sessions();
        let shown = sign_in(&sessions);
        let stream = watch(&sessions, &shown, true);
        for _ in 0..89 * 24 {
            clock.advance(HOUR_MS);
            assert!(stream.renew_if_watched());
        }
        clock.advance(24 * HOUR_MS);
        assert!(!stream.renew_if_watched(), "the 90-day limit still ends it");
        assert!(!acts(&sessions, &shown));
    }

    #[test]
    fn a_watched_tab_is_not_the_one_a_ninth_sign_in_ends() {
        let (clock, sessions) = lifetime_sessions();
        let live: Vec<_> = (0..MAX_DEVICES)
            .map(|_| {
                let session = sign_in(&sessions);
                keep_for_test(&sessions, &session);
                clock.advance(1_000);
                session
            })
            .collect();
        let stream = watch(&sessions, &live[0], true);
        clock.advance(HOUR_MS);
        assert!(stream.renew_if_watched());
        clock.advance(1_000);
        let ninth = sign_in(&sessions);
        assert!(
            acts(&sessions, &live[0]),
            "the watched oldest sign-in stays"
        );
        assert!(!acts(&sessions, &live[1]), "the idle one ended instead");
        assert!(acts(&sessions, &ninth));
    }

    #[test]
    fn a_device_ends_after_thirty_days_idle_and_after_ninety_days_in_all() {
        let (clock, sessions) = lifetime_sessions();
        let idle = sign_in(&sessions);
        let busy = sign_in(&sessions);
        assert!(acts(&sessions, &idle) && acts(&sessions, &busy));
        clock.advance(30 * DAY_MS - 1);
        assert!(
            acts(&sessions, &busy),
            "30 days less a millisecond is still live"
        );
        clock.advance(1);
        assert!(!acts(&sessions, &idle), "30 days idle ends it");
        assert!(acts(&sessions, &busy), "use renewed the other");

        for _ in 0..2 {
            clock.advance(29 * DAY_MS);
            assert!(acts(&sessions, &busy), "use keeps it for 90 days");
        }
        clock.advance(2 * DAY_MS);
        assert!(
            !acts(&sessions, &busy),
            "90 days in all end it, however busy"
        );
    }

    #[test]
    fn a_device_that_makes_no_request_within_a_day_of_signing_in_ends() {
        let (clock, sessions) = lifetime_sessions();
        let unused = sign_in(&sessions);
        let used = sign_in(&sessions);
        assert!(acts(&sessions, &used));
        clock.advance(DAY_MS - 1);
        assert!(acts(&sessions, &used));
        clock.advance(1);
        assert!(
            !acts(&sessions, &unused),
            "a device nobody used ends at a day"
        );
        assert!(acts(&sessions, &used), "a device in use does not");
        assert_eq!(
            sessions.gone_reason(&unused.cookie),
            Some(GoneReason::Expired)
        );
    }

    #[test]
    fn the_cookie_is_kept_for_the_soonest_limit_that_applies() {
        let (clock, sessions) = lifetime_sessions();
        let session = sign_in(&sessions);
        assert_eq!(
            session.cookie_max_age_s,
            24 * 3600,
            "a device nobody has used is kept a day"
        );
        let identity = |s: &Joined| {
            sessions
                .enter(LIFETIME_HOST, LIFETIME_ORIGIN, s)
                .expect("live")
        };
        clock.advance(6 * HOUR_MS);
        assert_eq!(identity(&session).cookie_max_age_s(), Some(30 * 24 * 3600));
        for _ in 0..3 {
            clock.advance(29 * DAY_MS);
            identity(&session);
        }
        clock.advance(2 * DAY_MS);
        let near_the_end = 90 * 24 * 3600 - (6 + 89 * 24) * 3600;
        assert_eq!(identity(&session).cookie_max_age_s(), Some(near_the_end));
    }

    #[test]
    fn signing_out_ends_the_session_and_closes_its_sockets() {
        let (_, sessions) = lifetime_sessions();
        let leaving = sign_in(&sessions);
        let staying = sign_in(&sessions);
        let socket = sessions
            .authorizer()
            .stream(LIFETIME_HOST, LIFETIME_ORIGIN, &leaving)
            .expect("a stream slot");
        let kept = sessions
            .authorizer()
            .stream(LIFETIME_HOST, LIFETIME_ORIGIN, &staying)
            .expect("a stream slot");
        assert!(socket.is_valid() && kept.is_valid());

        sessions
            .standing(LIFETIME_HOST, LIFETIME_ORIGIN, &leaving)
            .expect("known")
            .sign_out();

        assert!(!socket.is_valid(), "its socket closes at the next check");
        assert!(!acts(&sessions, &leaving), "its cookie acts for no one");
        assert!(
            sessions
                .standing(LIFETIME_HOST, LIFETIME_ORIGIN, &leaving)
                .is_none(),
            "the daemon now says the session is unknown"
        );
        assert!(kept.is_valid() && acts(&sessions, &staying));
    }
}

#[cfg(test)]
mod device_tests {
    use std::sync::atomic::AtomicBool;

    use dudes_proto::{Entry, Record};
    use dudes_supervisor::World;
    use dudes_supervisor::testing::FakeClock;
    use dudes_types::Lsn;

    use super::test_support::Joined;
    use super::*;
    use crate::floor_browser::Browser;

    const HOST: &str = "127.0.0.1:43301";
    const ORIGIN: &str = "http://127.0.0.1:43301";
    const DAY: i64 = DAY_MS;

    /// The journal as the daemon keeps it, for the part that matters here: records go through the
    /// supervisor's own fold, so a restart seeds the table from what that fold built.
    struct Journal {
        clock: Arc<FakeClock>,
        world: Mutex<(u64, World)>,
        changes: Mutex<Vec<FloorDeviceChange>>,
        events: Mutex<Vec<DeviceEvent>>,
        refuse: AtomicBool,
        during_removal: Mutex<Option<Box<dyn Fn() + Send>>>,
    }

    impl Journal {
        fn new(clock: &Arc<FakeClock>) -> Arc<Self> {
            Arc::new(Self {
                clock: Arc::clone(clock),
                world: Mutex::new((0, World::default())),
                changes: Mutex::new(Vec::new()),
                events: Mutex::new(Vec::new()),
                refuse: AtomicBool::new(false),
                during_removal: Mutex::new(None),
            })
        }

        fn devices(&self) -> BTreeMap<Hex32, DeviceRow> {
            self.world.lock().expect("world").1.floor_devices.clone()
        }

        fn changes(&self) -> Vec<FloorDeviceChange> {
            self.changes.lock().expect("changes").clone()
        }

        fn events(&self) -> Vec<DeviceEvent> {
            self.events.lock().expect("events").clone()
        }

        fn removals(&self) -> Vec<FloorDeviceRemoval> {
            self.changes()
                .into_iter()
                .filter_map(|change| match change {
                    FloorDeviceChange::Remove { reason, .. } => Some(reason),
                    _ => None,
                })
                .collect()
        }
    }

    impl DeviceSink for Journal {
        fn record(&self, change: FloorDeviceChange) -> bool {
            if self.refuse.load(Ordering::Acquire) {
                return false;
            }
            if matches!(change, FloorDeviceChange::Remove { .. })
                && let Some(check) = &*self.during_removal.lock().expect("hook")
            {
                check();
            }
            let record = match change.clone() {
                FloorDeviceChange::Enroll {
                    device,
                    origin,
                    name,
                    key,
                    cookie,
                } => Record::FloorDeviceKeyEnrolled {
                    device,
                    key,
                    origin,
                    name,
                    cookie,
                },
                FloorDeviceChange::FirstUse(device) => Record::FloorDeviceFirstUsed { device },
                FloorDeviceChange::Use(device) => Record::FloorDeviceUsed { device },
                FloorDeviceChange::Acknowledge(device) => {
                    Record::FloorDeviceAcknowledged { device }
                }
                FloorDeviceChange::Remove { device, reason } => {
                    Record::FloorDeviceRemoved { device, reason }
                }
            };
            let mut world = self.world.lock().expect("world");
            world.0 += 1;
            let lsn = Lsn::from_parts(1, world.0).expect("lsn");
            world.1.apply(
                lsn,
                &Entry {
                    at: self.clock.now(),
                    record,
                },
            );
            self.changes.lock().expect("changes").push(change);
            true
        }

        fn announce(&self, event: &DeviceEvent) {
            self.events.lock().expect("events").push(event.clone());
        }
    }

    struct Daemon {
        clock: Arc<FakeClock>,
        journal: Arc<Journal>,
        sessions: FloorSessions,
    }

    fn daemon() -> Daemon {
        let clock = Arc::new(FakeClock::frozen(1_000_000));
        let journal = Journal::new(&clock);
        let sessions = restart_from(&clock, &journal);
        Daemon {
            clock,
            journal,
            sessions,
        }
    }

    /// A new run over the same journal: a fresh table seeded from what the journal holds.
    fn restart_from(clock: &Arc<FakeClock>, journal: &Arc<Journal>) -> FloorSessions {
        let sessions = FloorSessions::new(HOST.parse().expect("loopback"), Arc::clone(clock));
        sessions.install_sink(Arc::clone(journal) as Arc<dyn DeviceSink>);
        sessions.seed(&journal.devices());
        sessions
    }

    fn sign_in_named(sessions: &FloorSessions, name: &str) -> Joined {
        let issued = sessions.issue().expect("a link");
        sessions
            .join_as(HOST, ORIGIN, &issued.bootstrap, Browser::new(), name)
            .expect("a device")
    }

    fn sign_in(sessions: &FloorSessions) -> Joined {
        sign_in_named(sessions, "Chrome on macOS")
    }

    fn acts(sessions: &FloorSessions, device: &Joined) -> bool {
        sessions.enter(HOST, ORIGIN, device).is_some()
    }

    fn key(device: &Joined) -> [u8; 32] {
        digest(device.cookie.as_bytes())
    }

    fn id_of(sessions: &FloorSessions, device: &Joined) -> Hex32 {
        sessions.0.registry.lock().sessions[&key(device)].id
    }

    fn identity(sessions: &FloorSessions, device: &Joined) -> SessionIdentity {
        sessions.standing(HOST, ORIGIN, device).expect("live")
    }

    #[test]
    fn a_device_is_durable_before_its_cookie_leaves_and_the_journal_holds_digests_only() {
        let d = daemon();
        d.journal.refuse.store(true, Ordering::Release);
        let issued = d.sessions.issue().expect("a link");
        assert!(
            d.sessions.join(HOST, ORIGIN, &issued.bootstrap).is_none(),
            "no durable enrolment, no cookie"
        );
        assert_eq!(d.sessions.registry_counts_for_test().1, 0);

        d.journal.refuse.store(false, Ordering::Release);
        let device = sign_in(&d.sessions);
        let rows = d.journal.devices();
        assert_eq!(rows.len(), 1);
        let row = rows.values().next().expect("one device");
        assert_eq!(row.cookie.bytes(), key(&device));
        assert_eq!(row.key.as_deref(), Some(device.browser.key()));
        assert_eq!(row.name, "Chrome on macOS");
        let written = format!("{:?}", d.journal.changes());
        assert!(!written.contains(&device.cookie));
    }

    #[test]
    fn a_name_loses_control_characters_is_capped_and_defaults_to_browser() {
        let d = daemon();
        let long = "é".repeat(100);
        let capped = "é".repeat(MAX_NAME_CHARS);
        for (sent, expected) in [
            ("Chrome\u{7}\non\u{1b}[0m macOS", "Chromeon[0m macOS"),
            ("Chro\u{202e}me\u{200b} on\u{feff} Mac", "Chrome on Mac"),
            ("   ", "Browser"),
            ("", "Browser"),
            (long.as_str(), capped.as_str()),
        ] {
            let device = sign_in_named(&d.sessions, sent);
            let id = id_of(&d.sessions, &device);
            assert_eq!(d.journal.devices()[&id].name, expected, "{sent:?}");
        }
        let issued = d.sessions.issue().expect("a link");
        let browser = Browser::new();
        let body =
            serde_json::json!({ "link": issued.bootstrap, "key": browser.key() }).to_string();
        assert!(
            d.sessions
                .exchange_signed(HOST, ORIGIN, &browser, &body)
                .is_ok(),
            "a page that sends no name still signs in"
        );
        let id = Hex32::from(browser.device());
        assert_eq!(d.journal.devices()[&id].name, "Browser");
    }

    #[test]
    fn first_use_is_journaled_at_once_and_later_use_at_most_once_a_day() {
        let d = daemon();
        let device = sign_in(&d.sessions);
        assert_eq!(d.journal.changes().len(), 1, "enrolled and nothing else");
        assert!(acts(&d.sessions, &device));
        for _ in 0..50 {
            d.clock.advance(60_000);
            assert!(acts(&d.sessions, &device));
        }
        let uses = |journal: &Journal| {
            journal
                .changes()
                .iter()
                .filter(|change| {
                    matches!(
                        change,
                        FloorDeviceChange::FirstUse(_) | FloorDeviceChange::Use(_)
                    )
                })
                .count()
        };
        assert_eq!(uses(&d.journal), 1, "an hour of requests is one record");
        d.clock.advance(DAY);
        assert!(acts(&d.sessions, &device));
        assert!(acts(&d.sessions, &device));
        assert_eq!(uses(&d.journal), 2, "the next day adds one");
        let row = &d.journal.devices()[&id_of(&d.sessions, &device)];
        assert!(row.first_used.is_some() && row.last_used > row.first_used);
    }

    #[test]
    fn a_removed_device_is_refused_with_its_reason_and_its_socket_closes() {
        let d = daemon();
        let leaving = sign_in(&d.sessions);
        let staying = sign_in(&d.sessions);
        assert!(acts(&d.sessions, &leaving) && acts(&d.sessions, &staying));
        let socket = d
            .sessions
            .authorizer()
            .stream(HOST, ORIGIN, &leaving)
            .expect("a stream slot");
        let other = d
            .sessions
            .authorizer()
            .stream(HOST, ORIGIN, &staying)
            .expect("a stream slot");

        let sessions = d.sessions.clone();
        *d.journal.during_removal.lock().expect("hook") = Some(Box::new(move || {
            assert_eq!(
                sessions.registry_counts_for_test().1,
                2,
                "the removal is made durable while the device is still signed in"
            );
        }));
        let id = id_of(&d.sessions, &leaving);
        assert_eq!(
            d.sessions
                .remove_device(id.into(), FloorDeviceRemoval::Removed),
            Removal::Removed
        );

        assert!(!socket.is_valid(), "its socket closes");
        assert!(!acts(&d.sessions, &leaving), "its next request is refused");
        assert_eq!(
            d.sessions.gone_reason(&leaving.cookie),
            Some(GoneReason::Removed)
        );
        assert_eq!(
            d.sessions.gone_reason(&staying.cookie),
            None,
            "a device still signed in has no reason to give"
        );
        assert!(other.is_valid() && acts(&d.sessions, &staying));
        assert_eq!(d.journal.removals(), [FloorDeviceRemoval::Removed]);
        assert!(d.journal.events().contains(&DeviceEvent::Removed {
            device: id.into(),
            reason: FloorDeviceRemoval::Removed
        }));
        assert_eq!(
            d.sessions
                .remove_device(id.into(), FloorDeviceRemoval::Removed),
            Removal::Unknown,
            "a second removal finds nothing"
        );
    }

    #[test]
    fn a_removal_the_journal_cannot_take_leaves_the_device_signed_in() {
        let d = daemon();
        let device = sign_in(&d.sessions);
        assert!(acts(&d.sessions, &device));
        d.journal.refuse.store(true, Ordering::Release);
        let id = id_of(&d.sessions, &device);
        assert_eq!(
            d.sessions
                .remove_device(id.into(), FloorDeviceRemoval::Removed),
            Removal::Unavailable
        );
        assert_eq!(
            identity(&d.sessions, &device).sign_out(),
            Removal::Unavailable
        );
        d.journal.refuse.store(false, Ordering::Release);
        assert!(acts(&d.sessions, &device), "still signed in");
    }

    #[test]
    fn signing_out_journals_that_reason_and_tells_the_pages() {
        let d = daemon();
        let device = sign_in(&d.sessions);
        assert_eq!(identity(&d.sessions, &device).sign_out(), Removal::Removed);
        assert_eq!(d.journal.removals(), [FloorDeviceRemoval::SignedOut]);
        assert_eq!(
            d.sessions.gone_reason(&device.cookie),
            None,
            "the person who signed out knows why"
        );
        assert!(matches!(
            d.journal.events().last(),
            Some(DeviceEvent::Removed {
                reason: FloorDeviceRemoval::SignedOut,
                ..
            })
        ));
    }

    /// Eight devices: `unused` never made a request, `new` did but nobody kept it, and the rest
    /// are kept. Returns them in sign-in order.
    fn full_table(d: &Daemon, unused: &[usize], new: &[usize]) -> Vec<Joined> {
        let devices: Vec<_> = (0..MAX_DEVICES)
            .map(|index| {
                let device = sign_in(&d.sessions);
                if !unused.contains(&index) {
                    assert!(acts(&d.sessions, &device));
                    if !new.contains(&index) {
                        d.sessions
                            .0
                            .registry
                            .lock()
                            .sessions
                            .get_mut(&key(&device))
                            .expect("signed in")
                            .acknowledged = true;
                    }
                }
                d.clock.advance(1_000);
                device
            })
            .collect();
        assert_eq!(d.sessions.registry_counts_for_test().1, MAX_DEVICES);
        devices
    }

    fn survivors(d: &Daemon, devices: &[Joined]) -> Vec<bool> {
        devices
            .iter()
            .map(|device| d.sessions.gone_reason(&device.cookie).is_none())
            .collect()
    }

    #[test]
    fn a_ninth_sign_in_removes_a_never_used_device_first() {
        let d = daemon();
        let devices = full_table(&d, &[5], &[2]);
        let ninth = sign_in(&d.sessions);
        assert_eq!(
            d.sessions.gone_reason(&devices[5].cookie),
            Some(GoneReason::Replaced),
            "the device nobody used goes before one still flagged new or the oldest"
        );
        assert_eq!(
            survivors(&d, &devices)
                .iter()
                .filter(|alive| **alive)
                .count(),
            7
        );
        assert!(acts(&d.sessions, &ninth));
        assert_eq!(d.journal.removals(), [FloorDeviceRemoval::Replaced]);
        assert_eq!(d.sessions.registry_counts_for_test().1, MAX_DEVICES);
    }

    #[test]
    fn with_none_unused_a_ninth_sign_in_removes_one_still_flagged_new() {
        let d = daemon();
        let devices = full_table(&d, &[], &[6]);
        sign_in(&d.sessions);
        assert_eq!(
            d.sessions.gone_reason(&devices[6].cookie),
            Some(GoneReason::Replaced),
            "the flagged one goes though it is the newest and the oldest was used least"
        );
        assert!(acts(&d.sessions, &devices[0]));
    }

    #[test]
    fn with_all_kept_a_ninth_sign_in_removes_the_least_recently_used_and_ties_go_to_the_older() {
        let d = daemon();
        let devices = full_table(&d, &[], &[]);
        // Use every device but the third, and the first two at one instant: the third goes.
        d.clock.advance(1_000);
        for (index, device) in devices.iter().enumerate() {
            if index != 2 {
                assert!(acts(&d.sessions, device));
            }
        }
        sign_in(&d.sessions);
        assert_eq!(
            d.sessions.gone_reason(&devices[2].cookie),
            Some(GoneReason::Replaced)
        );

        let tie = daemon();
        let devices = full_table(&tie, &[], &[]);
        tie.clock.advance(1_000);
        for device in &devices {
            assert!(acts(&tie.sessions, device));
        }
        sign_in(&tie.sessions);
        assert_eq!(
            tie.sessions.gone_reason(&devices[0].cookie),
            Some(GoneReason::Replaced),
            "equal use goes to the older sign-in"
        );
        assert_eq!(
            survivors(&tie, &devices)
                .iter()
                .filter(|alive| **alive)
                .count(),
            7
        );
    }

    #[test]
    fn a_device_nobody_used_goes_at_a_day_and_one_in_use_goes_at_thirty_and_ninety_days() {
        let d = daemon();
        let unused = sign_in(&d.sessions);
        let idle = sign_in(&d.sessions);
        let steady = sign_in(&d.sessions);
        assert!(acts(&d.sessions, &idle) && acts(&d.sessions, &steady));
        d.clock.advance(DAY - 1);
        assert!(acts(&d.sessions, &steady));
        d.clock.advance(1);
        assert!(!acts(&d.sessions, &unused));
        assert_eq!(d.journal.removals(), [FloorDeviceRemoval::NeverUsed]);

        d.clock.advance(29 * DAY - 1);
        assert!(acts(&d.sessions, &steady), "inside 30 days of its last use");
        d.clock.advance(1);
        assert!(!acts(&d.sessions, &idle), "30 days without use");
        assert!(d.journal.removals().contains(&FloorDeviceRemoval::Expired));

        for _ in 0..2 {
            d.clock.advance(29 * DAY);
            assert!(acts(&d.sessions, &steady));
        }
        d.clock.advance(3 * DAY);
        assert!(!acts(&d.sessions, &steady), "90 days in all");
        assert_eq!(
            d.sessions.gone_reason(&steady.cookie),
            Some(GoneReason::Expired)
        );
    }

    #[test]
    fn an_open_link_lives_a_minute_a_printed_one_ten_and_printing_never_blocks_opening() {
        let d = daemon();
        let open = d.sessions.issue_for(LinkPurpose::Open).expect("open");
        let printed = d.sessions.issue_for(LinkPurpose::Print).expect("printed");
        assert_eq!(open.expires_in_s, 60);
        assert_eq!(printed.expires_in_s, 600);
        d.clock.advance(OPEN_LINK_MS - 1);
        let still = d.sessions.issue_for(LinkPurpose::Open).expect("open");
        d.clock.advance(1);
        assert!(
            d.sessions.join(HOST, ORIGIN, &open.bootstrap).is_none(),
            "an open link is gone at 60 s"
        );
        assert!(
            d.sessions.join(HOST, ORIGIN, &printed.bootstrap).is_some(),
            "a printed link is not"
        );
        d.clock.advance(PRINTED_LINK_MS - OPEN_LINK_MS);
        assert!(d.sessions.join(HOST, ORIGIN, &still.bootstrap).is_none());
        let printed = d.sessions.issue_for(LinkPurpose::Print).expect("printed");
        d.clock.advance(PRINTED_LINK_MS);
        assert!(
            d.sessions.join(HOST, ORIGIN, &printed.bootstrap).is_none(),
            "a printed link is gone at 10 minutes"
        );

        let printed: Vec<_> = (0..MAX_PENDING_PRINTED_LINKS)
            .map(|_| {
                d.clock.advance(1_000);
                d.sessions.issue_for(LinkPurpose::Print).expect("printed")
            })
            .collect();
        for _ in 0..MAX_PENDING_OPEN_LINKS {
            d.sessions.issue_for(LinkPurpose::Open).expect("open");
        }
        assert_eq!(
            d.sessions.issue_for(LinkPurpose::Open),
            Err(IssueError::Capacity)
        );
        let newest = d
            .sessions
            .issue_for(LinkPurpose::Print)
            .expect("a new print");
        assert!(
            d.sessions
                .join(HOST, ORIGIN, &printed[0].bootstrap)
                .is_none(),
            "a new print replaces the oldest unused printed link"
        );
        assert!(
            d.sessions
                .join(HOST, ORIGIN, &printed[1].bootstrap)
                .is_some()
        );
        assert!(d.sessions.join(HOST, ORIGIN, &newest.bootstrap).is_some());
    }

    #[test]
    fn devices_survive_a_restart_and_a_removed_one_stays_removed() {
        let d = daemon();
        let kept = sign_in(&d.sessions);
        let removed = sign_in(&d.sessions);
        let unused = sign_in(&d.sessions);
        assert!(acts(&d.sessions, &kept) && acts(&d.sessions, &removed));
        let removed_id = id_of(&d.sessions, &removed);
        d.sessions
            .remove_device(removed_id.into(), FloorDeviceRemoval::Removed);
        d.clock.advance(1_000);
        assert!(acts(&d.sessions, &kept));
        d.sessions.invalidate();

        let after = restart_from(&d.clock, &d.journal);
        assert!(acts(&after, &kept), "the same cookie and key still act");
        assert!(!acts(&after, &removed), "a removed device stays removed");
        assert_eq!(
            after.gone_reason(&removed.cookie),
            Some(GoneReason::Removed),
            "and still hears why"
        );
        assert!(
            acts(&after, &unused),
            "an unused device is still within its day"
        );
        let listed = after.devices(None);
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|view| view.name == "Chrome on macOS"));
    }

    #[test]
    fn a_restart_ends_a_device_that_ran_out_and_one_signed_in_on_another_port() {
        let d = daemon();
        let unused = sign_in(&d.sessions);
        let fresh = sign_in(&d.sessions);
        assert!(acts(&d.sessions, &fresh));
        d.sessions.invalidate();
        d.clock.advance(DAY);

        let after = restart_from(&d.clock, &d.journal);
        assert!(!acts(&after, &unused), "its day is over");
        assert!(
            d.journal
                .removals()
                .contains(&FloorDeviceRemoval::NeverUsed)
        );

        let moved = FloorSessions::new(
            "127.0.0.1:43302".parse().expect("loopback"),
            Arc::clone(&d.clock),
        );
        moved.install_sink(Arc::clone(&d.journal) as Arc<dyn DeviceSink>);
        moved.seed(&d.journal.devices());
        assert_eq!(moved.devices(None).len(), 0, "another origin cannot use it");
        assert_eq!(
            d.journal.removals(),
            [FloorDeviceRemoval::NeverUsed],
            "and nothing is journaled for it"
        );

        let home = restart_from(&d.clock, &d.journal);
        assert!(acts(&home, &fresh), "it works again on its own origin");
    }

    #[test]
    fn a_restart_over_the_limit_removes_the_surplus_as_replaced() {
        let d = daemon();
        for n in 1..=9_u8 {
            d.clock.advance(1_000);
            let browser = Browser::seeded(u64::from(n));
            assert!(d.journal.record(FloorDeviceChange::Enroll {
                device: browser.device().into(),
                origin: ORIGIN.to_owned(),
                name: "Browser".to_owned(),
                key: browser.key().to_owned(),
                cookie: Hex32::new([n; 32]),
            }));
        }
        let after = restart_from(&d.clock, &d.journal);
        let listed = after.devices(None);
        assert_eq!(listed.len(), MAX_DEVICES);
        assert!(
            listed
                .iter()
                .all(|view| view.device != Browser::seeded(1).device()),
            "the oldest unused one goes"
        );
        assert_eq!(d.journal.removals(), [FloorDeviceRemoval::Replaced]);
    }

    /// Append a record the way an older daemon wrote it, straight into the journal's fold.
    fn append_old(journal: &Journal, record: Record) {
        let mut world = journal.world.lock().expect("world");
        world.0 += 1;
        let lsn = Lsn::from_parts(1, world.0).expect("lsn");
        world.1.apply(
            lsn,
            &Entry {
                at: journal.clock.now(),
                record,
            },
        );
    }

    #[test]
    fn a_browser_from_before_signed_requests_ends_once_as_expired() {
        let d = daemon();
        let old_cookie = "C".repeat(43);
        append_old(
            &d.journal,
            Record::FloorDeviceEnrolled {
                device: Hex32::new([4; 32]),
                origin: ORIGIN.to_owned(),
                name: "Chrome on macOS".to_owned(),
                cookie: Hex32::new(digest(old_cookie.as_bytes())),
                secret: Hex32::new([6; 32]),
            },
        );
        let keyed = sign_in(&d.sessions);

        let after = restart_from(&d.clock, &d.journal);
        assert_eq!(
            after.devices(None).len(),
            1,
            "only the browser with a key is signed in"
        );
        assert_eq!(
            d.journal.removals(),
            [FloorDeviceRemoval::Expired],
            "the old sign-in ends once, as expired"
        );
        assert_eq!(
            after.gone_reason(&old_cookie),
            Some(GoneReason::Expired),
            "its page hears that its sign-in expired"
        );
        assert!(acts(&after, &keyed), "the browser with a key is untouched");

        let again = restart_from(&d.clock, &d.journal);
        assert_eq!(again.devices(None).len(), 1);
        assert_eq!(
            d.journal.removals(),
            [FloorDeviceRemoval::Expired],
            "a later start finds nothing left to end"
        );
        assert_eq!(again.gone_reason(&old_cookie), Some(GoneReason::Expired));
    }

    /// A request from `who` carrying the request id `n`, signed for the registry's time plus `skew`.
    fn attempt(d: &Daemon, who: &Joined, n: u64, skew: i64) -> Result<SessionIdentity, Refusal> {
        let mut id = [0_u8; 16];
        id[8..].copy_from_slice(&n.to_be_bytes());
        let signed = who.browser.sign_with_id(
            "POST",
            "/floor/test",
            ORIGIN,
            &[],
            d.sessions.now_ms() + skew,
            id,
        );
        d.sessions.authorize_onboard(
            &Presentation::new(HOST, ORIGIN, &who.cookie, "POST", "/floor/test", &[]).signed(
                &signed.origin,
                &signed.time,
                &signed.request,
                &signed.signature,
            ),
        )
    }

    #[test]
    fn a_device_remembers_at_most_1024_request_ids_and_each_for_two_minutes() {
        let d = daemon();
        let device = sign_in(&d.sessions);
        let start = Timestamp::from_unix_millis(d.sessions.now_ms());
        {
            // Signing a thousand requests in a debug build is slow, so the memory starts one
            // short of full, as a thousand and twenty-three earlier requests leave it. The
            // enrolment's own request holds one place already.
            let mut state = d.sessions.0.registry.lock();
            let session = state
                .sessions
                .get_mut(&digest(device.cookie.as_bytes()))
                .expect("signed in");
            for n in 1..MAX_REQUEST_IDS - 1 {
                let mut id = [0_u8; 16];
                id[8..].copy_from_slice(&(n as u64).to_be_bytes());
                session.requests.seen.push((start, id));
            }
        }
        assert!(attempt(&d, &device, 0, 0).is_ok(), "the last free place");
        assert_eq!(
            attempt(&d, &device, 5000, 0).err(),
            Some(Refusal::Signature),
            "a full memory refuses a new id until old ones age out"
        );
        assert_eq!(
            attempt(&d, &device, 1, 0).err(),
            Some(Refusal::Signature),
            "and an id it holds"
        );
        d.clock.advance(REQUEST_MEMORY_MS - 1);
        assert!(
            attempt(&d, &device, 5000, 0).is_err(),
            "still within two minutes"
        );
        d.clock.advance(1);
        assert!(
            attempt(&d, &device, 5000, 0).is_ok(),
            "two minutes on, there is room again"
        );
        assert!(
            attempt(&d, &device, 1, 0).is_ok(),
            "and an id that aged out may be used again"
        );
    }

    #[test]
    fn a_request_must_be_within_a_minute_of_the_daemons_clock_either_way() {
        let d = daemon();
        let device = sign_in(&d.sessions);
        assert!(attempt(&d, &device, 1, SIGNATURE_WINDOW_MS).is_ok());
        assert!(attempt(&d, &device, 2, -SIGNATURE_WINDOW_MS).is_ok());
        assert_eq!(
            attempt(&d, &device, 3, SIGNATURE_WINDOW_MS + 1).err(),
            Some(Refusal::Signature)
        );
        assert_eq!(
            attempt(&d, &device, 4, -SIGNATURE_WINDOW_MS - 1).err(),
            Some(Refusal::Signature)
        );
        assert!(
            attempt(&d, &device, 3, 0).is_ok(),
            "a refused request does not use up its id"
        );
    }

    #[test]
    fn an_exchange_the_key_did_not_sign_is_refused_and_keeps_the_link() {
        let d = daemon();
        let issued = d.sessions.issue().expect("a link");
        let browser = Browser::new();
        let body = browser.enrolment(&issued.bootstrap, "Chrome on macOS");
        let signed = |by: &Browser, at: i64| {
            by.presentation(
                HOST,
                ORIGIN,
                "",
                "POST",
                "/floor/session",
                body.as_bytes(),
                at,
            )
        };
        let now = d.sessions.now_ms();
        let unsigned =
            Presentation::new(HOST, ORIGIN, "", "POST", "/floor/session", body.as_bytes());
        assert_eq!(
            d.sessions.exchange(&unsigned).err(),
            Some(ExchangeRefusal::Signature),
            "no signature"
        );
        assert_eq!(
            d.sessions.exchange(&signed(&Browser::new(), now)).err(),
            Some(ExchangeRefusal::Signature),
            "another key's signature"
        );
        assert_eq!(
            d.sessions
                .exchange(&signed(&browser, now - SIGNATURE_WINDOW_MS - 1))
                .err(),
            Some(ExchangeRefusal::Signature),
            "a signature outside the window"
        );
        let good = browser.presentation(
            HOST,
            ORIGIN,
            "",
            "POST",
            "/floor/session",
            body.as_bytes(),
            now,
        );
        assert!(
            d.sessions.exchange(&good).is_ok(),
            "the refused attempts left the link unspent"
        );
        assert_eq!(
            d.journal.changes().len(),
            1,
            "only the signed exchange enrolled"
        );
    }

    #[test]
    fn an_exchange_without_a_valid_key_is_refused_enrols_nothing_and_keeps_the_link() {
        let d = daemon();
        let issued = d.sessions.issue().expect("a link");
        let key = Browser::new().key().to_owned();
        let signer = Browser::new();
        let refuse = |body: String| {
            d.sessions
                .exchange_signed(HOST, ORIGIN, &signer, &body)
                .err()
        };
        for body in [
            serde_json::json!({ "link": issued.bootstrap }),
            serde_json::json!({ "link": issued.bootstrap, "key": "" }),
            serde_json::json!({ "link": issued.bootstrap, "key": "AAAA" }),
            serde_json::json!({ "link": issued.bootstrap, "key": &key[1..] }),
            serde_json::json!({ "link": issued.bootstrap, "key": format!("{key}AA") }),
            serde_json::json!({ "link": issued.bootstrap, "key": key, "secret": "x" }),
        ] {
            assert_eq!(
                refuse(body.to_string()),
                Some(ExchangeRefusal::Malformed),
                "{body}"
            );
        }
        assert_eq!(
            refuse("not json".to_owned()),
            Some(ExchangeRefusal::Malformed)
        );
        assert_eq!(d.journal.changes().len(), 0, "nothing was enrolled");
        assert_eq!(d.sessions.registry_counts_for_test(), (1, 0));
        assert!(
            d.sessions.join(HOST, ORIGIN, &issued.bootstrap).is_some(),
            "the link was not spent by the refusals"
        );
    }

    #[test]
    fn a_key_that_signs_in_again_replaces_its_own_device() {
        let d = daemon();
        let browser = Browser::seeded(77);
        let issue = || d.sessions.issue().expect("a link").bootstrap;
        let first = d
            .sessions
            .join_as(HOST, ORIGIN, &issue(), Browser::seeded(77), "Chrome")
            .expect("the first sign-in");
        let second = d
            .sessions
            .join_as(HOST, ORIGIN, &issue(), browser, "Chrome")
            .expect("the second sign-in");
        assert_ne!(first.cookie, second.cookie);
        assert_eq!(d.sessions.devices(None).len(), 1, "one browser, one device");
        assert!(!acts(&d.sessions, &first), "the old cookie opens nothing");
        assert!(acts(&d.sessions, &second));
        assert_eq!(d.journal.removals(), [FloorDeviceRemoval::Replaced]);
        assert_eq!(
            d.sessions.gone_reason(&first.cookie),
            Some(GoneReason::Replaced)
        );
        let again = restart_from(&d.clock, &d.journal);
        assert_eq!(
            again.devices(None).len(),
            1,
            "a restart folds the later sign-in over the removed one"
        );
        assert!(acts(&again, &second) && !acts(&again, &first));
    }

    #[test]
    fn a_first_use_the_journal_refused_is_recorded_by_the_next_request() {
        let d = daemon();
        let device = sign_in(&d.sessions);
        d.journal.refuse.store(true, Ordering::Release);
        assert!(acts(&d.sessions, &device), "the request is still admitted");
        d.journal.refuse.store(false, Ordering::Release);
        assert!(acts(&d.sessions, &device));
        let uses = d
            .journal
            .changes()
            .iter()
            .filter(|change| matches!(change, FloorDeviceChange::FirstUse(_)))
            .count();
        assert_eq!(uses, 1, "the second request recorded it");
        let id = id_of(&d.sessions, &device);
        assert!(d.journal.devices()[&id].first_used.is_some());
    }

    #[test]
    fn a_removal_that_raced_an_expiry_still_counts_as_removed() {
        let d = daemon();
        let device = sign_in(&d.sessions);
        assert!(acts(&d.sessions, &device));
        let id = id_of(&d.sessions, &device);
        let sessions = d.sessions.clone();
        let clock = Arc::clone(&d.clock);
        *d.journal.during_removal.lock().expect("hook") = Some(Box::new(move || {
            clock.advance(IDLE_EXPIRES_MS + 1);
            let registry::Locked { mut state, now } = sessions.0.registry.lock_at_now();
            state.prune(now);
        }));
        assert_eq!(
            d.sessions
                .remove_device(id.into(), FloorDeviceRemoval::Removed),
            Removal::Removed
        );
        assert_eq!(
            d.journal.removals(),
            [FloorDeviceRemoval::Removed],
            "journaled once, under the reason asked for"
        );
    }

    #[test]
    fn a_new_device_is_announced_and_only_another_device_can_keep_it() {
        let d = daemon();
        let first = sign_in(&d.sessions);
        let second = sign_in_named(&d.sessions, "Firefox on Linux");
        let second_id = id_of(&d.sessions, &second);
        assert!(matches!(
            d.journal.events().last(),
            Some(DeviceEvent::Enrolled { device, name, .. })
                if *device == DeviceId::from(second_id) && name == "Firefox on Linux"
        ));
        assert!(d.sessions.devices(None).iter().all(|view| view.new));

        let own = identity(&d.sessions, &second);
        assert_eq!(d.sessions.keep_device(second_id.into(), &own), Keep::Own);
        let other = identity(&d.sessions, &first);
        assert_eq!(d.sessions.keep_device(second_id.into(), &other), Keep::Kept);
        assert_eq!(d.sessions.keep_device(second_id.into(), &other), Keep::Kept);
        assert_eq!(
            d.journal
                .changes()
                .iter()
                .filter(|change| matches!(change, FloorDeviceChange::Acknowledge(_)))
                .count(),
            1,
            "keeping twice records once"
        );
        let listed = d.sessions.devices(Some(&own));
        let me = listed.iter().find(|view| view.this).expect("this device");
        assert_eq!(me.device, DeviceId::from(second_id));
        assert!(!me.new);
        assert_eq!(listed.iter().filter(|view| view.new).count(), 1);
        assert_eq!(
            d.sessions.keep_device(DeviceId::new([7; 32]), &other),
            Keep::Unknown
        );
    }

    #[test]
    fn listing_marks_use_and_lists_the_newest_sign_in_first() {
        let d = daemon();
        let older = sign_in_named(&d.sessions, "Older");
        d.clock.advance(5_000);
        let newer = sign_in_named(&d.sessions, "Newer");
        assert!(acts(&d.sessions, &older));
        let listed = d.sessions.devices(Some(&identity(&d.sessions, &older)));
        assert_eq!(
            listed
                .iter()
                .map(|view| view.name.as_str())
                .collect::<Vec<_>>(),
            ["Newer", "Older"]
        );
        assert!(listed[1].this && !listed[0].this);
        assert!(listed[0].first_used.is_none() && listed[0].last_used.is_none());
        assert!(listed[1].first_used.is_some() && listed[1].last_used.is_some());
        let _ = newer;
    }
}

#[cfg(test)]
mod carry_tests {
    use dudes_supervisor::testing::FakeClock;

    use super::test_support::Joined;
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

    fn sign_in(run: &Run) -> Joined {
        let issued = run.carry.sessions().issue().expect("link");
        run.carry
            .sessions()
            .join(HOST, ORIGIN, &issued.bootstrap)
            .expect("device")
    }

    fn key(exchange: &Joined) -> [u8; 32] {
        digest(exchange.cookie.as_bytes())
    }

    fn created(run: &Run, exchange: &Joined, creates: u32, assigns: u32, checks: u32) {
        for (counted, times) in [(Counted::Create, creates), (Counted::Assign, assigns)] {
            for _ in 0..times {
                let turn = run
                    .tickets
                    .begin(key(exchange), Some(counted), false)
                    .expect("admitted");
                turn.succeeded();
            }
        }
        run.checks.seed(Hex32::new(key(exchange)), checks);
    }

    /// The rows the journal would hold for the devices a run has signed in.
    fn rows_of(from: &Run) -> BTreeMap<Hex32, DeviceRow> {
        from.carry.sessions().rows_for_test()
    }

    /// A run over the same devices: the journal brings them back with the same cookies, so the
    /// export is all that has to carry.
    fn same_devices(from: &Run, clock: &Arc<FakeClock>) -> Run {
        let next = run(clock, HOST);
        next.carry.sessions().seed(&rows_of(from));
        next
    }

    #[test]
    fn export_names_digests_times_and_counts_and_no_secret() {
        let clock = Arc::new(FakeClock::frozen(1_000));
        let old = run(&clock, HOST);
        let one = sign_in(&old);
        created(&old, &one, 3, 2, 5);
        clock.advance(60_000);

        let exported = old.carry.export(Some(OLD_RUN));
        let text = serde_json::to_string(&exported).expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
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
            ],
            "no secret digest leaves: the journal holds the devices"
        );
        assert_eq!(row["ticket_creates"], 3);
        assert_eq!(row["ticket_assigns"], 2);
        assert_eq!(row["check_runs"], 5);
        assert_eq!(row["created"], 1_000);
        assert_eq!(row["last_used"], 1_000, "the export refreshes nothing");
        assert_eq!(value["run"], OLD_RUN);
        assert_eq!(value["origin"], ORIGIN);
        assert!(!text.contains(&one.cookie));
        let digest = row["digest"].as_str().expect("digest");
        assert!(
            !format!("{exported:?}").contains(digest),
            "Debug lists no digest"
        );
        assert_eq!(digest.len(), 64, "the digest, not the cookie");
    }

    #[test]
    fn an_import_restores_the_counts_of_the_devices_the_new_run_holds() {
        let clock = Arc::new(FakeClock::frozen(1_000));
        let old = run(&clock, HOST);
        let one = sign_in(&old);
        created(&old, &one, 3, 2, 5);
        let exported = old.carry.export(Some(OLD_RUN));
        let new = same_devices(&old, &clock);
        old.carry.sessions().invalidate();
        assert_eq!(new.carry.import(Some(NEW_RUN), &exported), 1);
        assert_eq!(new.tickets.counts(key(&one)), (3, 2));
        assert_eq!(new.checks.started(Hex32::new(key(&one))), 5);
    }

    #[test]
    fn an_export_from_the_previous_release_imports_without_error_and_restores_nothing() {
        let clock = Arc::new(FakeClock::frozen(1_000));
        let exported = SessionExport {
            run: OLD_RUN.to_owned(),
            origin: ORIGIN.to_owned(),
            sessions: vec![ExportedSession {
                digest: Hex32::new(digest(b"a-cookie-of-the-previous-release")),
                created: Timestamp::from_unix_millis(500),
                last_used: Timestamp::from_unix_millis(900),
                ticket_creates: 4,
                ticket_assigns: 1,
                check_runs: 2,
            }],
        };
        let new = run(&clock, HOST);
        assert_eq!(new.carry.import(Some(NEW_RUN), &exported), 0);
        assert_eq!(
            new.tickets
                .counts(digest(b"a-cookie-of-the-previous-release")),
            (0, 0)
        );
        assert_eq!(new.carry.sessions().registry_counts_for_test().1, 0);
    }

    #[test]
    fn an_import_refuses_or_skips_what_it_must() {
        let clock = Arc::new(FakeClock::frozen(10_000));
        let old = run(&clock, HOST);
        let one = sign_in(&old);
        created(&old, &one, 1, 1, 1);
        let exported = old.carry.export(Some(OLD_RUN));

        assert_eq!(
            old.carry.import(Some(OLD_RUN), &exported),
            0,
            "the run that exported"
        );
        let new = same_devices(&old, &clock);
        assert_eq!(new.carry.import(Some(OLD_RUN), &exported), 0, "same run id");

        assert_eq!(new.carry.import(Some(NEW_RUN), &exported), 1);
        assert_eq!(
            new.carry.import(Some(NEW_RUN), &exported),
            0,
            "a second import admits nothing"
        );

        let other = same_devices(&old, &clock);
        let mut moved = exported.clone();
        moved.origin = "http://127.0.0.1:43212".to_owned();
        assert_eq!(
            other.carry.import(Some(NEW_RUN), &moved),
            0,
            "another origin"
        );
        assert_eq!(other.tickets.counts(key(&one)), (0, 0));
    }

    #[test]
    fn any_import_that_passes_the_run_check_uses_the_runs_one_chance() {
        let clock = Arc::new(FakeClock::frozen(1_000));
        let old = run(&clock, HOST);
        sign_in(&old);
        let exported = old.carry.export(Some(OLD_RUN));
        let new = same_devices(&old, &clock);
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
        let new = same_devices(&old, &clock);
        assert_eq!(new.carry.import(None, &exported), 0);
        assert_eq!(
            new.carry.import(Some(NEW_RUN), &exported),
            1,
            "a refused import without an identity did not use the run's chance"
        );
    }

    #[test]
    fn an_ended_run_exports_nothing() {
        let clock = Arc::new(FakeClock::frozen(1_000));
        let old = run(&clock, HOST);
        sign_in(&old);
        old.carry.sessions().invalidate();
        assert!(old.carry.export(Some(OLD_RUN)).sessions.is_empty());
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
        carry.sessions().seed(&rows_of(&old));
        let mut baselines = baselines;
        assert_eq!(carry.import(Some(NEW_RUN), &exported), 1);

        assert_eq!(
            tickets.begin(key(&one), Some(Counted::Create), false).err(),
            Some("limit_reached"),
            "the create allowance was spent before the upgrade"
        );
        assert!(
            tickets
                .begin(key(&one), Some(Counted::Assign), false)
                .is_ok(),
            "control: an assign still has one left"
        );
        assert_eq!(
            baselines.start_browser(None, Hex32::new(key(&one))),
            Err(Limit::Exhausted),
            "the check-run cap was reached before the upgrade"
        );
    }
}
