//! The dedicated route for the seven provenance-conditioned effects.
//!
//! The complete conditioned set is the partition in [`R5Method`]. `agent.ask` remains a bounded question
//! under its own cap and open-question slot and never enters this router.

use dudes_proto::rpc::pty::PtyId;
use dudes_proto::rpc::{ErrorCode, Method, RefusalGate, RefusedData, RpcError};
use dudes_proto::{Actor, ConditionedMethod, ConditionedTarget, PayloadDigest, Record};
use dudes_types::Lsn;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::auth::{PeerIdentity, PeerProvenance, PeerRole};

/// Maximum decoded UTF-8 bytes carried by one `agent.steer` effect.
pub const MAX_AGENT_STEER_TEXT_BYTES: usize = crate::codec::MAX_FRAME_BYTES / 16;
/// Maximum decoded UTF-8 bytes carried by one `agent.answer` effect.
pub const MAX_AGENT_ANSWER_TEXT_BYTES: usize = crate::codec::MAX_FRAME_BYTES / 16;
/// Maximum decoded UTF-8 bytes carried by one `mail.send` body.
pub const MAX_MAIL_SEND_BODY_BYTES: usize = crate::codec::MAX_FRAME_BYTES / 16;
/// Maximum base64-decoded bytes carried by one `pty.write` effect.
pub const MAX_PTY_WRITE_BYTES: usize = crate::codec::MAX_FRAME_BYTES / 16;

/// The ceiling on the decoded text of a `memory.append`, in **bytes** — a fact the disposition
/// table reads, so an over-cap append is quarantined with its digest rather than applied.
///
/// # Derived, because a fourth number is a fourth thing to keep in step
///
/// Three constants already bounded this text before any value was picked: the transport's
/// admission ceiling [`MAX_FRAME_BYTES`] at 1 MiB; the journal's on-disk format ceiling at 16 MiB
/// (`crates/dudes-journal/src/frame.rs`), which is kept deliberately unrelated to the
/// first — the two answer different questions and are not aliases; and the journal's 4 KiB inline
/// threshold, which decides whether an admitted body stays in `Record::MemoryAppend.text` or
/// becomes a referenced file. A hand-picked fourth number is a fourth thing to keep in step with
/// those, and nothing fails on the day it stops being in step. So the value below is a division of
/// the first, and the two assertions after it hold it against that division and against the third.
///
/// **A sixteenth of the frame cap, for [`MAX_ASK_TEXT_BYTES`]'s reason.** The daemon re-wraps an
/// admitted text into a `journal.appended` notification and the writer preflights *that* against
/// [`MAX_FRAME_BYTES`] (`crates/dudes-journal/src/writer.rs:185`) — after the call was admitted,
/// decoded and carried. A text that only just fitted on the way in can compose an envelope that
/// does not fit on the way out, and the refusal then lands on the wrong side of the admission
/// boundary, which is what invariant 4 forbids.
///
/// **The margin is 2.67×, measured, not the sixteen the ratio suggests.** Sixteen cap-lengths fit
/// one frame only for bytes JSON copies through unchanged. The cap counts bytes and validates no
/// content, so the worst pattern it admits is a control character: `U+0001` is one byte in and six
/// out as `\u0001`. A cap-length text of them composes a 393,444-byte `journal.appended` frame
/// against the 1,048,576-byte cap, where the same text in ASCII composes 65,764. That — under
/// threefold, not fifteenfold — is the real headroom an envelope has to grow into.
/// [`tests::a_memory_at_the_cap_fits_one_journal_appended_frame`] composes both frames through the
/// real fan-out and brackets the headroom, so the claim is checked rather than restated; the two
/// byte counts here are what it measures.
///
/// **64 KiB is the size every conditioned effect is capped at**: quarantine keeps a digest rather
/// than the withheld bytes because the frame ceiling is 1 MiB and a 64 KiB body is 6.25% of it.
/// `MAX_FRAME_BYTES / 16` is 65,536 bytes. The derivation is therefore not a new opinion about
/// how large a memory may be; it keeps this cap equal to the other conditioned caps.
///
/// **What is capped is the decoded payload, not a params field.** For every conditioned method the
/// decoder hands the router the exact decoded byte sequence ([`DecodedEffect::new`]), and for
/// `memory.append` those are the JSON-decoded string's UTF-8 bytes with no normalisation.
/// Those are the bytes that get digested, journalled and — on `Apply` — written
/// as a body, so those are the bytes worth bounding; a cap read off `params.text` would bound a
/// field the decoder is free to reinterpret.
///
/// [`MAX_FRAME_BYTES`]: crate::codec::MAX_FRAME_BYTES
/// [`MAX_ASK_TEXT_BYTES`]: crate::methods::agent::MAX_ASK_TEXT_BYTES
pub const MAX_MEMORY_APPEND_TEXT_BYTES: usize = crate::codec::MAX_FRAME_BYTES / 16;

/// The derivation itself, checked **by the compiler on every build** rather than by a test.
///
/// **It is inert while the line above is that same division**, and saying so is the point rather
/// than an apology: what this catches is the edit the cap exists to prevent — the constant
/// re-spelled as a literal that no longer tracks the frame cap, which is how a cap and the ceiling
/// it was derived from drift apart in silence.
///
/// It is an **equality**, not a `* 16 <=` bound, because the doc above claims the two cannot drift
/// and a `<=` claims only that this one is not larger than a sixteenth. Under `<=`, `/32` and any
/// hand-picked value under 64 KiB pass while the prose keeps calling it the 16:1 ratio. *Measured
/// control:* re-spelling the definition as `8_192` — a value the old bound admitted — is
/// `error[E0080]` here, as are `131_072` and `4096`.
///
/// [`MAX_FRAME_BYTES`]: crate::codec::MAX_FRAME_BYTES
const _: () = assert!(
    MAX_MEMORY_APPEND_TEXT_BYTES == crate::codec::MAX_FRAME_BYTES / 16,
    "the memory.append cap is a sixteenth of the frame cap and not merely no larger than one, \
     because the daemon re-wraps an admitted text into a journal.appended notification the writer \
     preflights against that cap — and a refusal there is after admission, which invariant 4 forbids"
);

/// The floor, which is the half a derivation does not have.
///
/// The equality above pins this cap to whatever [`MAX_FRAME_BYTES`] is, and says nothing about
/// whether the quotient is still a usable size. Divide a 64 KiB frame cap by sixteen and it lands
/// exactly on 4 KiB — the journal's inline threshold, at or under which an admitted text stays inline in the
/// record and over which the daemon writes a referenced body
/// (`crates/dudes-proto/src/record.rs:472-479`). A cap at or below that threshold does not merely
/// make appends smaller, it makes the referenced-body arm unreachable — every admitted append
/// inline — which is a silent change to the journal's storage rule. This assertion is what stops a
/// **shrinking** frame cap arriving as a quiet re-decision of it.
///
/// [`MAX_FRAME_BYTES`]: crate::codec::MAX_FRAME_BYTES
const _: () = assert!(
    MAX_MEMORY_APPEND_TEXT_BYTES > 4 * 1024,
    "The journal puts a body over 4 KiB out of line; a cap at or under that threshold makes the \
     referenced-body arm unreachable rather than merely making an admitted memory smaller"
);

const _: () = assert!(
    MAX_AGENT_STEER_TEXT_BYTES == crate::codec::MAX_FRAME_BYTES / 16,
    "the agent.steer text cap is exactly one sixteenth of the frame cap"
);
const _: () = assert!(
    MAX_AGENT_ANSWER_TEXT_BYTES == crate::codec::MAX_FRAME_BYTES / 16,
    "the agent.answer text cap is exactly one sixteenth of the frame cap"
);
const _: () = assert!(
    MAX_MAIL_SEND_BODY_BYTES == crate::codec::MAX_FRAME_BYTES / 16,
    "the mail.send body cap is exactly one sixteenth of the frame cap"
);
const _: () = assert!(
    MAX_PTY_WRITE_BYTES == crate::codec::MAX_FRAME_BYTES / 16,
    "the pty.write decoded-byte cap is exactly one sixteenth of the frame cap"
);
const _: () = assert!(
    MAX_MAIL_SEND_BODY_BYTES > 4 * 1024,
    "the mail body cap must leave the journal's over-4-KiB storage arm reachable"
);
const _: () = assert!(
    4 * MAX_PTY_WRITE_BYTES.div_ceil(3) < crate::codec::MAX_FRAME_BYTES,
    "a pty.write at the decoded-byte cap must fit its base64 representation in one frame"
);

const fn payload_ceiling(method: ConditionedMethod) -> Option<usize> {
    match method {
        ConditionedMethod::AgentSteer => Some(MAX_AGENT_STEER_TEXT_BYTES),
        ConditionedMethod::AgentAnswer => Some(MAX_AGENT_ANSWER_TEXT_BYTES),
        ConditionedMethod::PtyWrite => Some(MAX_PTY_WRITE_BYTES),
        ConditionedMethod::MailSend => Some(MAX_MAIL_SEND_BODY_BYTES),
        ConditionedMethod::MemoryAppend => Some(MAX_MEMORY_APPEND_TEXT_BYTES),
        ConditionedMethod::AgentAttach | ConditionedMethod::AgentDetach => None,
    }
}

/// A quarantine or suppression reason: the operator role held no applying authority.
pub const AUTHORITY_UNAVAILABLE: &str = "operator authority unavailable";
/// A quarantine or suppression reason: the peer's provenance did not match its role.
pub const PROVENANCE_MISMATCH: &str = "peer provenance does not satisfy the effect";
/// A quarantine or suppression reason: the peer resolved to no role that may cause the effect.
pub const ROLE_MISMATCH: &str = "resolved role does not satisfy the effect";
/// A quarantine or suppression reason: the resolved agent does not own the target.
pub const TARGET_MISMATCH: &str = "resolved agent does not own the target";
const ATTACHMENT_NOT_OWNED: &str = "connection does not own the PTY attachment";
/// The quarantine reason for a memory text over [`MAX_MEMORY_APPEND_TEXT_BYTES`].
pub const TEXT_OVER_CEILING: &str = "memory text exceeds the append ceiling";
const PAYLOAD_OVER_CEILING: &str = "decoded payload exceeds the method ceiling";

/// The pure result consumed by the conditioned router.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Disposition {
    Apply,
    Suppress(&'static str),
    Quarantine(&'static str),
}

/// The daemon-owned source for connection-bound operator effect authority.
///
/// The source begins either in the pre-agent acquisition epoch or sealed, according to the
/// replayed world. [`Self::seal`] and acquisition take the same lock: a racing accept therefore
/// either obtains its lease before the seal or observes the seal and obtains nothing. Sealing is
/// irreversible for the life of this value.
///
/// This type is deliberately not an authentication input. The server consults it only after the
/// transport has independently resolved peer provenance, and the dispatcher still applies
/// the peer classification and the method requirement before the conditioned route. No request, token,
/// credential, environment value, argv value, file, or journal field carries this object.
#[derive(Debug)]
pub struct OperatorAuthoritySource {
    state: Arc<Mutex<AuthorityState>>,
}

#[derive(Debug)]
struct AuthorityState {
    sealed: bool,
    live_leases: usize,
}

impl OperatorAuthoritySource {
    /// Build the source from the authoritative replay result.
    ///
    /// Any recorded agent seals acquisition before the listener starts. A zero-agent replay opens
    /// the short pre-agent epoch; [`Self::seal`] closes it before the first typed hire is enqueued.
    #[must_use]
    pub fn from_replayed_agents(has_recorded_agent: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(AuthorityState {
                sealed: has_recorded_agent,
                live_leases: 0,
            })),
        }
    }

    /// Irreversibly end the pre-agent acquisition epoch.
    pub fn seal(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sealed = true;
    }

    /// Whether acquisition has been irreversibly sealed.
    #[must_use]
    pub fn is_sealed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sealed
    }

    /// How many accepted connections still hold a live lease.
    ///
    /// This is diagnostic state, never an admission input. It exists so lifecycle tests can prove
    /// EOF and halt actually revoke rather than merely stop using an authority that remains live.
    #[must_use]
    pub fn live_leases(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .live_leases
    }

    /// Attempt one acquisition at the accepted-connection boundary.
    ///
    /// `NoRecordedAgent` is only corroboration: it cannot construct the lease, and every other
    /// provenance is denied even while the epoch is open. The construction below is the sole
    /// production constructor for [`OperatorEffectAuthority`].
    fn acquire(&self, provenance: PeerProvenance) -> Option<OperatorEffectAuthority> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.sealed || provenance != PeerProvenance::NoRecordedAgent {
            return None;
        }
        state.live_leases = state
            .live_leases
            .checked_add(1)
            .expect("the connection cap bounds live authority leases");
        Some(OperatorEffectAuthority {
            live: AtomicBool::new(true),
            source: Arc::clone(&self.state),
        })
    }
}

/// One opaque, connection-bound, single-owner lease.
///
/// No `Clone`, `Serialize`, public field, or public constructor exists. Only
/// [`OperatorAuthoritySource::acquire`] constructs it, and [`ConditionedConnectionContext`] owns
/// it until that connection closes.
#[derive(Debug)]
struct OperatorEffectAuthority {
    live: AtomicBool,
    source: Arc<Mutex<AuthorityState>>,
}

impl OperatorEffectAuthority {
    fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }

    fn revoke(&self) {
        if !self.live.swap(false, Ordering::AcqRel) {
            return;
        }
        let mut source = self.source.lock().unwrap_or_else(PoisonError::into_inner);
        source.live_leases = source
            .live_leases
            .checked_sub(1)
            .expect("a live authority lease was counted at acquisition");
    }
}

impl Drop for OperatorEffectAuthority {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// Facts already resolved or derived before the pure disposition lookup.
#[derive(Clone, Copy)]
struct Facts<'a> {
    role: PeerRole,
    provenance: PeerProvenance,
    target: TargetFact,
    authority: Option<&'a OperatorEffectAuthority>,
    owns_pty: bool,
    /// Whether the decoded payload is longer than this method's [`payload_ceiling`].
    ///
    /// Derived for every method from its total ceiling mapping and read only by the bounded rows.
    /// Carrying it here rather than checking the length above [`decide`] keeps that table the only
    /// thing that decides — a length checked above it would be a third pre-disposition stop, and
    /// the effect table admits exactly one.
    over_cap: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetFact {
    Agent(dudes_types::AgentId),
    Pty,
    Mail,
}

/// The exhaustive effect-local disposition table. No I/O, no clock, no ambient state.
fn decide(method: ConditionedMethod, facts: Facts<'_>) -> Disposition {
    if !target_matches(method, facts.target) {
        return if method == ConditionedMethod::MemoryAppend {
            Disposition::Quarantine(TARGET_MISMATCH)
        } else {
            Disposition::Suppress(TARGET_MISMATCH)
        };
    }
    if method == ConditionedMethod::MemoryAppend {
        return decide_memory(facts);
    }

    if facts.role != PeerRole::Operator {
        return Disposition::Suppress(ROLE_MISMATCH);
    }
    if facts.authority.is_none() {
        return Disposition::Suppress(AUTHORITY_UNAVAILABLE);
    }
    if facts.provenance != PeerProvenance::NoRecordedAgent {
        return Disposition::Suppress(PROVENANCE_MISMATCH);
    }
    if facts.over_cap
        && matches!(
            method,
            ConditionedMethod::AgentSteer
                | ConditionedMethod::AgentAnswer
                | ConditionedMethod::PtyWrite
                | ConditionedMethod::MailSend
        )
    {
        return Disposition::Suppress(PAYLOAD_OVER_CEILING);
    }
    if method == ConditionedMethod::PtyWrite && !facts.owns_pty {
        return Disposition::Suppress(ATTACHMENT_NOT_OWNED);
    }
    Disposition::Apply
}

fn decide_memory(facts: Facts<'_>) -> Disposition {
    let TargetFact::Agent(target) = facts.target else {
        return Disposition::Quarantine(TARGET_MISMATCH);
    };
    // Before the role arms, because an over-cap text is refused whoever sent it and this reason is
    // the one worth journalling: the record's `PayloadDigest` carries the observed `decoded_len`,
    // so the quarantine is the evidence of what was refused and how long it was.
    if facts.over_cap {
        return Disposition::Quarantine(TEXT_OVER_CEILING);
    }
    match facts.role {
        PeerRole::Agent { agent } => {
            if target != agent {
                Disposition::Quarantine(TARGET_MISMATCH)
            } else if facts.provenance != PeerProvenance::Agent(agent) {
                Disposition::Quarantine(PROVENANCE_MISMATCH)
            } else {
                Disposition::Apply
            }
        }
        PeerRole::Operator => {
            if facts.authority.is_none() {
                Disposition::Quarantine(AUTHORITY_UNAVAILABLE)
            } else if facts.provenance != PeerProvenance::NoRecordedAgent {
                Disposition::Quarantine(PROVENANCE_MISMATCH)
            } else {
                Disposition::Apply
            }
        }
        PeerRole::Unproven => Disposition::Quarantine(ROLE_MISMATCH),
    }
}

const fn target_matches(method: ConditionedMethod, target: TargetFact) -> bool {
    matches!(
        (method, target),
        (
            ConditionedMethod::AgentSteer
                | ConditionedMethod::AgentAnswer
                | ConditionedMethod::AgentAttach
                | ConditionedMethod::AgentDetach
                | ConditionedMethod::MemoryAppend,
            TargetFact::Agent(_)
        ) | (ConditionedMethod::PtyWrite, TargetFact::Pty)
            | (ConditionedMethod::MailSend, TargetFact::Mail)
    )
}

/// Typed params and the target/payload bytes derived by a conditioned method's decoder.
///
/// This type deliberately carries neither peer identity, provenance, role nor actor. Those facts
/// come from the authenticated connection and are joined only inside the router.
pub struct DecodedEffect<M> {
    params: M,
    target: ConditionedTarget,
    payload: Option<Vec<u8>>,
}

impl<M> DecodedEffect<M> {
    /// A decoder's output. `payload` is the exact decoded byte sequence; attach/detach use `None`.
    #[must_use]
    pub fn new(params: M, target: ConditionedTarget, payload: Option<Vec<u8>>) -> Self {
        Self {
            params,
            target,
            payload,
        }
    }

    /// The derived target, for a decoder's own unit tests.
    ///
    /// Test-only and crate-private: a decoder is worth testing without standing a router up, and
    /// nothing outside this crate may read a decoded effect without going through the disposition.
    #[cfg(test)]
    pub(crate) const fn target_for_test(&self) -> &ConditionedTarget {
        &self.target
    }

    /// The decoded bytes, for a decoder's own unit tests. See [`Self::target_for_test`].
    #[cfg(test)]
    pub(crate) fn payload_for_test(&self) -> &[u8] {
        self.payload.as_deref().unwrap_or_default()
    }
}

/// Sanitized connection facts for transport admission and the conditioned route.
///
/// The server creates this once from transport-reported identity and resolved provenance. Its
/// fields and constructors stay crate-private: neither a general [`crate::Call`] nor an effect
/// sink can receive the raw peer identity or manufacture an actor.
pub(crate) struct ConditionedConnectionContext {
    provenance: PeerProvenance,
    transport_pid: TransportPid,
    peer_actor: Option<PeerActor>,
    authority: Option<OperatorEffectAuthority>,
    attachment: Mutex<Option<AttachmentBinding>>,
    hang_up: Option<crate::server::HangUp>,
    #[cfg(test)]
    owns_pty: bool,
}

#[derive(Clone, Copy)]
enum TransportPid {
    NotApplicable,
    Reported,
    Unreadable,
}

impl TransportPid {
    const fn from_peer(peer: PeerIdentity) -> Self {
        match peer.pid {
            crate::auth::PeerPid::Reported { .. } => Self::Reported,
            crate::auth::PeerPid::Unreadable => Self::Unreadable,
        }
    }
}

struct AttachmentBinding {
    agent: dudes_types::AgentId,
    pty: PtyId,
    cleanup: Arc<dyn Fn(dudes_types::AgentId, PtyId) + Send + Sync>,
}

struct PeerActor {
    pid: u32,
    uid: Option<u32>,
}

impl ConditionedConnectionContext {
    pub(crate) fn from_peer(peer: PeerIdentity, provenance: PeerProvenance) -> Self {
        Self::from_accepted_peer(peer, provenance, None)
    }

    /// Build the context in the accepted connection's own frame, optionally acquiring from the
    /// daemon's source.
    ///
    /// "Accepted" is the boundary this names, not a thread: the connection is admitted and its slot
    /// is charged before this runs, and no request of its has been read yet. It runs on that
    /// connection's thread, so a lease acquired here may find an epoch the accept thread would have
    /// found open — fail-closed, and stated at the call site in `server.rs`.
    pub(crate) fn from_accepted_peer(
        peer: PeerIdentity,
        provenance: PeerProvenance,
        source: Option<&OperatorAuthoritySource>,
    ) -> Self {
        let peer_actor = peer
            .pid
            .reported()
            .and_then(|pid| u32::try_from(pid).ok())
            .map(|pid| PeerActor { pid, uid: peer.uid });
        Self {
            provenance,
            transport_pid: TransportPid::from_peer(peer),
            peer_actor,
            authority: source.and_then(|source| source.acquire(provenance)),
            attachment: Mutex::new(None),
            hang_up: None,
            #[cfg(test)]
            owns_pty: false,
        }
    }

    /// Carry the transport's probe for whether this peer has gone.
    pub(crate) fn with_hang_up(mut self, hang_up: crate::server::HangUp) -> Self {
        self.hang_up = Some(hang_up);
        self
    }

    /// Whether this peer has gone, as far as its transport can tell.
    pub(crate) fn hang_up(&self) -> crate::server::HangUp {
        self.hang_up
            .clone()
            .unwrap_or_else(crate::server::HangUp::never)
    }

    pub(crate) const fn without_peer(provenance: PeerProvenance) -> Self {
        Self {
            provenance,
            transport_pid: TransportPid::NotApplicable,
            peer_actor: None,
            authority: None,
            attachment: Mutex::new(None),
            hang_up: None,
            #[cfg(test)]
            owns_pty: false,
        }
    }

    /// Build the otherwise impossible authority-plus-failing-provenance cell for deletion mutants.
    ///
    /// The lease is still acquired only through [`OperatorAuthoritySource::acquire`] in the real
    /// pre-agent epoch; this helper merely pairs it with a scripted resolved column. It is absent
    /// from production builds and cannot construct or expose [`OperatorEffectAuthority`].
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn for_authority_mutation(
        peer: PeerIdentity,
        provenance: PeerProvenance,
        source: &OperatorAuthoritySource,
    ) -> Option<Self> {
        let authority = source.acquire(PeerProvenance::NoRecordedAgent)?;
        let peer_actor = peer
            .pid
            .reported()
            .and_then(|pid| u32::try_from(pid).ok())
            .map(|pid| PeerActor { pid, uid: peer.uid });
        Some(Self {
            provenance,
            transport_pid: TransportPid::from_peer(peer),
            peer_actor,
            authority: Some(authority),
            attachment: Mutex::new(None),
            hang_up: None,
            #[cfg(test)]
            owns_pty: false,
        })
    }

    pub(crate) const fn provenance(&self) -> PeerProvenance {
        self.provenance
    }

    pub(crate) const fn transport_pid_is_unreadable(&self) -> bool {
        matches!(self.transport_pid, TransportPid::Unreadable)
    }

    pub(crate) fn peer_pid(&self) -> Option<u32> {
        self.peer_actor.as_ref().map(|actor| actor.pid)
    }

    pub(crate) fn peer_attribution(&self) -> Option<(u32, Option<u32>)> {
        self.peer_actor.as_ref().map(|actor| (actor.pid, actor.uid))
    }

    fn authority(&self) -> Option<&OperatorEffectAuthority> {
        self.authority
            .as_ref()
            .filter(|authority| authority.is_live())
    }

    fn owns_pty(&self) -> bool {
        self.attachment
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
            || {
                #[cfg(test)]
                {
                    self.owns_pty
                }
                #[cfg(not(test))]
                {
                    false
                }
            }
    }

    /// Whether this connection owns **this exact** live attachment.
    ///
    /// [`Self::owns_pty`] answers "does this connection hold a slot", which is what `agent.attach`
    /// needs and what `pty.write` must not be given: a connection attached to agent A would then
    /// satisfy the ownership row for agent B's `PtyId`, which is precisely the sibling-typing
    /// the peer classification is meant to prevent. The comparison is against the binding `agent.attach`
    /// recorded, so this adds no second source of ownership truth.
    fn owns(&self, pty: PtyId) -> bool {
        self.attachment
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|binding| binding.pty == pty)
            || {
                #[cfg(test)]
                {
                    self.owns_pty
                }
                #[cfg(not(test))]
                {
                    false
                }
            }
    }

    fn bind_attachment(
        &self,
        agent: dudes_types::AgentId,
        pty: PtyId,
        cleanup: Arc<dyn Fn(dudes_types::AgentId, PtyId) + Send + Sync>,
    ) -> Result<(), ()> {
        let mut slot = self
            .attachment
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if slot.is_some() {
            return Err(());
        }
        *slot = Some(AttachmentBinding {
            agent,
            pty,
            cleanup,
        });
        Ok(())
    }

    fn release_attachment(&self) {
        let binding = self
            .attachment
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(binding) = binding {
            (binding.cleanup)(binding.agent, binding.pty);
        }
    }

    /// Revoke before transport teardown. [`Drop`] is the unwind backstop.
    pub(crate) fn revoke(&self) {
        self.release_attachment();
        if let Some(authority) = &self.authority {
            authority.revoke();
        }
    }

    fn actor_for(&self, role: &PeerRole) -> Option<RouterActor> {
        match role {
            PeerRole::Agent { agent } => Some(RouterActor::Agent { agent: *agent }),
            PeerRole::Operator => self.peer_actor.as_ref().map(|peer| RouterActor::Peer {
                pid: peer.pid,
                uid: peer.uid,
            }),
            PeerRole::Unproven => None,
        }
    }
}

impl Drop for ConditionedConnectionContext {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// Router-owned actor material. No caller or effect sink can construct or inspect it.
enum RouterActor {
    Peer { pid: u32, uid: Option<u32> },
    Agent { agent: dudes_types::AgentId },
}

impl RouterActor {
    fn record_actor(&self) -> Actor {
        match self {
            Self::Peer { pid, uid } => Actor::Peer {
                pid: *pid,
                uid: *uid,
            },
            Self::Agent { agent } => Actor::Agent { agent: *agent },
        }
    }
}

/// A single-use proof that the central router consumed `Apply`.
///
/// Fields and constructor are private and the capability is intentionally not `Clone`. A sink
/// receives it by value but can inspect only decoded params and target; raw request, disposition,
/// peer identity, provenance and actor remain unavailable.
pub struct AppliedEffect<M> {
    params: M,
    target: ConditionedTarget,
    payload: Option<Vec<u8>>,
    payload_digest: Option<PayloadDigest>,
    provenance: PeerProvenance,
    actor: RouterActor,
}

impl<M> AppliedEffect<M> {
    /// The centrally decoded params.
    #[must_use]
    pub fn params(&self) -> &M {
        &self.params
    }

    /// The centrally derived target.
    #[must_use]
    pub fn target(&self) -> &ConditionedTarget {
        &self.target
    }

    /// The exact decoded payload bytes supplied by the method decoder.
    #[must_use]
    pub fn payload(&self) -> Option<&[u8]> {
        self.payload.as_deref()
    }

    /// The router's digest of [`Self::payload`].
    #[must_use]
    pub const fn payload_digest(&self) -> Option<&PayloadDigest> {
        self.payload_digest.as_ref()
    }

    /// Consume the capability and return only the decoded params.
    #[must_use]
    pub fn into_params(self) -> M {
        self.params
    }
}

/// The identity-bearing view available only while the router calls its persistence adapter.
///
/// The effect sink receives [`AppliedEffect`], not this view. Only this interface can convert the
/// opaque router actor to the record's [`Actor`] or inspect the exact provenance to persist it.
pub struct AppliedPersistence<'a, M> {
    effect: &'a AppliedEffect<M>,
}

impl<M> AppliedPersistence<'_, M> {
    /// The centrally decoded params from which the adapter builds the domain record.
    #[must_use]
    pub fn params(&self) -> &M {
        &self.effect.params
    }

    /// The centrally derived target.
    #[must_use]
    pub fn target(&self) -> &ConditionedTarget {
        &self.effect.target
    }

    /// The exact decoded payload bytes supplied by the method decoder.
    #[must_use]
    pub fn payload(&self) -> Option<&[u8]> {
        self.effect.payload()
    }

    /// The router's digest of [`Self::payload`].
    #[must_use]
    pub const fn payload_digest(&self) -> Option<&PayloadDigest> {
        self.effect.payload_digest()
    }

    /// The connection's exact provenance for the new RPC-applied record.
    #[must_use]
    pub const fn provenance(&self) -> PeerProvenance {
        self.effect.provenance
    }

    /// Convert the opaque router-owned actor to the record actor.
    #[must_use]
    pub fn actor(&self) -> Actor {
        self.effect.actor.record_actor()
    }
}

/// One router-owned persistence operation.
///
/// Exactly one arm is present. The adapter appends/fsyncs the disposition record directly, or
/// uses the applied view to build and durably append its method-specific domain record. Returning
/// `Ok` acknowledges durability; only then can the effect sink run.
pub struct PersistenceOperation<'a, M> {
    disposition: Option<&'a Record>,
    applied: Option<&'a AppliedEffect<M>>,
}

mod receipt_private {
    pub trait Sealed {}
}

/// A persistence acknowledgement allowed to cross from the identity-bearing adapter to a sink.
///
/// The private supertrait makes the set closed: an adapter can return no value, or the exact LSN
/// acknowledged by the journal. It cannot smuggle actor, provenance, peer identity, or a request
/// wrapper into the effect continuation through the otherwise-generic receipt type.
pub trait PersistenceReceipt: receipt_private::Sealed {}

impl receipt_private::Sealed for () {}
impl PersistenceReceipt for () {}
impl receipt_private::Sealed for Lsn {}
impl PersistenceReceipt for Lsn {}
impl receipt_private::Sealed for PtyId {}
impl PersistenceReceipt for PtyId {}
impl receipt_private::Sealed for dudes_proto::PtyWriteOutcome {}
impl PersistenceReceipt for dudes_proto::PtyWriteOutcome {}

impl<'a, M> PersistenceOperation<'a, M> {
    fn disposition(record: &'a Record) -> Self {
        Self {
            disposition: Some(record),
            applied: None,
        }
    }

    fn applied(effect: &'a AppliedEffect<M>) -> Self {
        Self {
            disposition: None,
            applied: Some(effect),
        }
    }

    /// The suppression/quarantine record, when this is a refused effect.
    #[must_use]
    pub const fn disposition_record(&self) -> Option<&Record> {
        self.disposition
    }

    /// The identity-bearing persistence view, when this is an applied effect.
    #[must_use]
    pub fn applied_effect(&self) -> Option<AppliedPersistence<'_, M>> {
        self.applied.map(|effect| AppliedPersistence { effect })
    }
}

/// Type-erased conditioned route stored by the dispatcher.
pub(crate) type ConditionedRoute = Box<
    dyn Fn(Option<&Value>, &PeerRole, &ConditionedConnectionContext) -> Result<Value, RpcError>
        + Send
        + Sync,
>;

/// The ownership fact [`decide`] reads, derived from the effect's own target.
///
/// One function so the two routes cannot disagree about what "owns" means. A `Pty` target asks
/// about **that** handle; every other target asks only whether this connection holds a slot, which
/// is the sense `agent.attach`'s "one live slot per connection" refusal uses.
fn owns_target(connection: &ConditionedConnectionContext, target: &ConditionedTarget) -> bool {
    match target {
        ConditionedTarget::Pty { pty } => connection.owns(*pty),
        ConditionedTarget::Agent { .. } | ConditionedTarget::Mail { .. } => connection.owns_pty(),
    }
}

fn applied_effect<M>(
    effect: DecodedEffect<M>,
    provenance: PeerProvenance,
    actor: RouterActor,
    payload_digest: Option<PayloadDigest>,
) -> AppliedEffect<M> {
    AppliedEffect {
        params: effect.params,
        target: effect.target,
        payload: effect.payload,
        payload_digest,
        provenance,
        actor,
    }
}

/// Build the one closure that decodes, decides and consumes a conditioned call.
pub(crate) fn route<M, R, D, P, S>(
    method: ConditionedMethod,
    decoder: D,
    persist: P,
    sink: S,
) -> ConditionedRoute
where
    M: 'static,
    R: PersistenceReceipt + 'static,
    D: Fn(Option<&Value>, &PeerRole) -> Result<DecodedEffect<M>, RpcError> + Send + Sync + 'static,
    P: for<'a> Fn(PersistenceOperation<'a, M>) -> Result<R, RpcError> + Send + Sync + 'static,
    S: Fn(AppliedEffect<M>, R) -> Result<Value, RpcError> + Send + Sync + 'static,
{
    Box::new(move |params, role, connection| {
        let provenance = connection.provenance;
        let effect = decoder(params, role)?;
        // A fact, not a stop. `None` is not "zero bytes admitted": a `memory.append` that derived
        // no payload is not over any ceiling and falls through to the `digest.is_none()` refusal
        // below, which is where a decoder that produced nothing belongs.
        let over_cap = payload_ceiling(method).is_some_and(|ceiling| {
            effect
                .payload
                .as_deref()
                .is_some_and(|bytes| bytes.len() > ceiling)
        });
        let digest = effect.payload.as_deref().map(payload_digest);
        let target = match &effect.target {
            ConditionedTarget::Agent { agent } => TargetFact::Agent(*agent),
            ConditionedTarget::Pty { .. } => TargetFact::Pty,
            ConditionedTarget::Mail { .. } => TargetFact::Mail,
        };
        if !target_matches(method, target) {
            return Err(invalid_params("target does not match conditioned method"));
        }
        if method == ConditionedMethod::MemoryAppend && digest.is_none() {
            return Err(invalid_params(
                "memory.append must derive one agent target and payload",
            ));
        }

        let disposition = decide(
            method,
            Facts {
                role: *role,
                provenance,
                target,
                authority: connection.authority(),
                owns_pty: owns_target(connection, &effect.target),
                over_cap,
            },
        );
        match disposition {
            Disposition::Suppress(reason) => {
                let record = Record::ConditionedEffectSuppressed {
                    method,
                    target: effect.target,
                    provenance,
                    reason: reason.into(),
                    payload: digest,
                };
                persist(PersistenceOperation::disposition(&record))?;
                Err(conditioned_refusal())
            }
            Disposition::Quarantine(reason) => {
                let TargetFact::Agent(agent) = target else {
                    return Err(invalid_params("quarantine requires an agent target"));
                };
                let Some(payload) = digest else {
                    return Err(invalid_params("quarantine requires payload bytes"));
                };
                let record = Record::MemoryAppendQuarantined {
                    agent,
                    payload,
                    provenance,
                    reason: reason.into(),
                };
                persist(PersistenceOperation::disposition(&record))?;
                Err(conditioned_refusal())
            }
            Disposition::Apply => {
                let Some(actor) = connection.actor_for(role) else {
                    return Err(conditioned_refusal());
                };
                let applied = applied_effect(effect, provenance, actor, digest);
                let receipt = persist(PersistenceOperation::applied(&applied))?;
                sink(applied, receipt)
            }
        }
    })
}

/// Build the connection-owning specialization of the central conditioned route for `agent.attach`.
pub(crate) fn attach_route<M, D, P>(
    decoder: D,
    persist: P,
    cleanup: Arc<dyn Fn(dudes_types::AgentId, PtyId) + Send + Sync>,
) -> ConditionedRoute
where
    M: 'static,
    D: Fn(Option<&Value>, &PeerRole) -> Result<DecodedEffect<M>, RpcError> + Send + Sync + 'static,
    P: for<'a> Fn(PersistenceOperation<'a, M>) -> Result<PtyId, RpcError> + Send + Sync + 'static,
{
    Box::new(move |params, role, connection| {
        let provenance = connection.provenance;
        let effect = decoder(params, role)?;
        let target = match &effect.target {
            ConditionedTarget::Agent { agent } => TargetFact::Agent(*agent),
            _ => return Err(invalid_params("agent.attach requires an agent target")),
        };
        let disposition = decide(
            ConditionedMethod::AgentAttach,
            Facts {
                role: *role,
                provenance,
                target,
                authority: connection.authority(),
                owns_pty: connection.owns_pty(),
                over_cap: false,
            },
        );
        match disposition {
            Disposition::Suppress(reason) => {
                let record = Record::ConditionedEffectSuppressed {
                    method: ConditionedMethod::AgentAttach,
                    target: effect.target,
                    provenance,
                    reason: reason.into(),
                    payload: None,
                };
                let _ = persist(PersistenceOperation::disposition(&record))?;
                Err(conditioned_refusal())
            }
            Disposition::Quarantine(_) => Err(invalid_params("agent.attach cannot quarantine")),
            Disposition::Apply => {
                if connection.owns_pty() {
                    return Err(conditioned_refusal());
                }
                let Some(actor) = connection.actor_for(role) else {
                    return Err(conditioned_refusal());
                };
                let applied = applied_effect(effect, provenance, actor, None);
                let ConditionedTarget::Agent { agent } = applied.target else {
                    unreachable!("the decoder target was checked above")
                };
                let pty = persist(PersistenceOperation::applied(&applied))?;
                if connection
                    .bind_attachment(agent, pty, Arc::clone(&cleanup))
                    .is_err()
                {
                    cleanup(agent, pty);
                    return Err(conditioned_refusal());
                }
                crate::methods::encode(
                    Method::AgentAttach,
                    &dudes_proto::rpc::agent::AttachResult { pty },
                )
            }
        }
    })
}

/// Revoke the exact connection binding before the daemon may close its PTY.
pub(crate) fn detach_route<M, D, P>(decoder: D, persist: P) -> ConditionedRoute
where
    M: 'static,
    D: Fn(Option<&Value>, &PeerRole) -> Result<DecodedEffect<M>, RpcError> + Send + Sync + 'static,
    P: for<'a> Fn(
            PersistenceOperation<'a, M>,
            Option<PtyId>,
        ) -> Result<Option<dudes_proto::PtyCloseOutcome>, RpcError>
        + Send
        + Sync
        + 'static,
{
    Box::new(move |params, role, connection| {
        let provenance = connection.provenance;
        let effect = decoder(params, role)?;
        let ConditionedTarget::Agent { agent } = &effect.target else {
            return Err(invalid_params("agent.detach requires an agent target"));
        };
        let agent = *agent;
        let disposition = decide(
            ConditionedMethod::AgentDetach,
            Facts {
                role: *role,
                provenance,
                target: TargetFact::Agent(agent),
                authority: connection.authority(),
                owns_pty: connection.owns_pty(),
                over_cap: false,
            },
        );
        let suppress = |reason: &'static str| {
            let record = Record::ConditionedEffectSuppressed {
                method: ConditionedMethod::AgentDetach,
                target: ConditionedTarget::Agent { agent },
                provenance,
                reason: reason.into(),
                payload: None,
            };
            let _ = persist(PersistenceOperation::disposition(&record), None)?;
            Err(conditioned_refusal())
        };
        match disposition {
            Disposition::Suppress(reason) => suppress(reason),
            Disposition::Quarantine(_) => Err(invalid_params("agent.detach cannot quarantine")),
            Disposition::Apply => {
                let Some(actor) = connection.actor_for(role) else {
                    return Err(conditioned_refusal());
                };
                // This mutex is also held through every admitted pty.write outcome. Taking the
                // binding here waits behind that write and bars every later one before close.
                let binding = {
                    let mut slot = connection
                        .attachment
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if slot.as_ref().is_some_and(|binding| binding.agent == agent) {
                        slot.take()
                    } else {
                        None
                    }
                };
                let Some(binding) = binding else {
                    return suppress(ATTACHMENT_NOT_OWNED);
                };
                let applied = applied_effect(effect, provenance, actor, None);
                let outcome = persist(PersistenceOperation::applied(&applied), Some(binding.pty));
                match outcome {
                    Ok(Some(
                        dudes_proto::PtyCloseOutcome::Closed
                        | dudes_proto::PtyCloseOutcome::AlreadyClosed,
                    )) => crate::methods::encode(Method::AgentDetach, &dudes_proto::rpc::Empty {}),
                    Ok(Some(dudes_proto::PtyCloseOutcome::Failed { .. })) => {
                        (binding.cleanup)(agent, binding.pty);
                        Err(conditioned_refusal())
                    }
                    Ok(None) => {
                        (binding.cleanup)(agent, binding.pty);
                        Err(invalid_params("agent.detach had no close outcome"))
                    }
                    Err(error) => {
                        (binding.cleanup)(agent, binding.pty);
                        Err(error)
                    }
                }
            }
        }
    })
}

/// Build the connection-owning specialization of the conditioned route for `pty.write`.
///
/// It is a route of its own for one reason the generic [`route`] cannot cover: a
/// `Partial` or `Failed` host outcome **revoke this connection's ownership**, and a sink receives
/// no connection. Everything else is the central router's — the decoder derives the target and the
/// exact bytes, the pure table decides, and only an acknowledged `Apply` can construct the private
/// applied capability.
///
/// `persist` owns the whole order of the effect and returns only after it: append and fsync
/// `PtyWriteRequested`, call the host **once**, append and fsync `PtyWriteCompleted`. The outcome
/// it returns is the host's confirmed answer, and it is a receipt rather than a sink argument so
/// that nothing between here and the journal can invent one.
pub(crate) fn write_route<M, D, P>(decoder: D, persist: P) -> ConditionedRoute
where
    M: 'static,
    D: Fn(Option<&Value>, &PeerRole) -> Result<DecodedEffect<M>, RpcError> + Send + Sync + 'static,
    P: for<'a> Fn(PersistenceOperation<'a, M>) -> Result<dudes_proto::PtyWriteOutcome, RpcError>
        + Send
        + Sync
        + 'static,
{
    Box::new(move |params, role, connection| {
        let provenance = connection.provenance;
        // Invalid params stop here, before any disposition exists: the third
        // pre-disposition path, and the one an undecodable `data` takes.
        let effect = decoder(params, role)?;
        let ConditionedTarget::Pty { .. } = &effect.target else {
            return Err(invalid_params("pty.write requires a pty target"));
        };
        let Some(digest) = effect.payload.as_deref().map(payload_digest) else {
            return Err(invalid_params(
                "pty.write must derive decoded payload bytes",
            ));
        };
        let over_cap = digest.decoded_len > MAX_PTY_WRITE_BYTES as u64;
        // Hold the exact connection binding through the durable host outcome. A detach takes this
        // same lock to revoke it, so an admitted write cannot queue behind the close request.
        let attachment = connection
            .attachment
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let ConditionedTarget::Pty { pty } = &effect.target else {
            unreachable!("checked above")
        };
        let owns_pty = attachment
            .as_ref()
            .is_some_and(|binding| binding.pty == *pty);
        #[cfg(test)]
        let owns_pty = owns_pty || connection.owns_pty;
        let disposition = decide(
            ConditionedMethod::PtyWrite,
            Facts {
                role: *role,
                provenance,
                target: TargetFact::Pty,
                authority: connection.authority(),
                owns_pty,
                over_cap,
            },
        );
        match disposition {
            Disposition::Suppress(reason) => {
                let record = Record::ConditionedEffectSuppressed {
                    method: ConditionedMethod::PtyWrite,
                    target: effect.target,
                    provenance,
                    reason: reason.into(),
                    payload: Some(digest),
                };
                let _ = persist(PersistenceOperation::disposition(&record))?;
                Err(conditioned_refusal())
            }
            Disposition::Quarantine(_) => Err(invalid_params("pty.write cannot quarantine")),
            Disposition::Apply => {
                let Some(actor) = connection.actor_for(role) else {
                    return Err(conditioned_refusal());
                };
                let applied = applied_effect(effect, provenance, actor, Some(digest));
                let outcome = persist(PersistenceOperation::applied(&applied))?;
                drop(attachment);
                // After the completion record, never before it: teardown goes behind
                // the durable outcome, so a crash between the host call and this point leaves the
                // unmatched request it describes rather than a revocation with no audit fact.
                if !matches!(outcome, dudes_proto::PtyWriteOutcome::Complete { .. }) {
                    connection.release_attachment();
                    return Err(conditioned_refusal());
                }
                crate::methods::encode(Method::PtyWrite, &dudes_proto::rpc::Empty {})
            }
        }
    })
}

/// Return the exact byte length and lowercase BLAKE3 digest of a payload.
#[must_use]
pub fn payload_digest(bytes: &[u8]) -> PayloadDigest {
    PayloadDigest {
        decoded_len: u64::try_from(bytes.len()).expect("usize fits into u64"),
        blake3: blake3::hash(bytes).to_hex().to_string(),
    }
}

fn invalid_params(message: &str) -> RpcError {
    RpcError {
        code: ErrorCode::InvalidParams.code(),
        message: message.into(),
        data: None,
    }
}

fn conditioned_refusal() -> RpcError {
    RpcError {
        code: ErrorCode::Refused.code(),
        message: "conditioned effect refused".into(),
        data: serde_json::to_value(RefusedData {
            gate: RefusalGate::ConditionedEffect,
        })
        .ok(),
    }
}

/// The complete, non-wire set of conditioned methods (`agent.ask` included).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum R5Method {
    /// One of the seven effects governed by the disposition table.
    Conditioned(ConditionedMethod),
    /// `agent.ask`, governed separately by its own cap and open-question slot.
    BoundedQuestion,
}

impl R5Method {
    /// Every conditioned row, in method-table order.
    pub const ALL: [Self; 8] = [
        Self::Conditioned(ConditionedMethod::AgentSteer),
        Self::Conditioned(ConditionedMethod::AgentAnswer),
        Self::Conditioned(ConditionedMethod::AgentAttach),
        Self::Conditioned(ConditionedMethod::AgentDetach),
        Self::Conditioned(ConditionedMethod::PtyWrite),
        Self::Conditioned(ConditionedMethod::MailSend),
        Self::Conditioned(ConditionedMethod::MemoryAppend),
        Self::BoundedQuestion,
    ];

    /// Classify a method into the complete conditioned partition.
    #[must_use]
    pub const fn from_method(method: Method) -> Option<Self> {
        match method {
            Method::AgentSteer => Some(Self::Conditioned(ConditionedMethod::AgentSteer)),
            Method::AgentAnswer => Some(Self::Conditioned(ConditionedMethod::AgentAnswer)),
            Method::AgentAttach => Some(Self::Conditioned(ConditionedMethod::AgentAttach)),
            Method::AgentDetach => Some(Self::Conditioned(ConditionedMethod::AgentDetach)),
            Method::PtyWrite => Some(Self::Conditioned(ConditionedMethod::PtyWrite)),
            Method::MailSend => Some(Self::Conditioned(ConditionedMethod::MailSend)),
            Method::MemoryAppend => Some(Self::Conditioned(ConditionedMethod::MemoryAppend)),
            Method::AgentAsk => Some(Self::BoundedQuestion),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use dudes_types::AgentId;

    use super::*;
    use crate::auth::{PeerIdentity, PeerPid, PeerProvenance, PeerRole};

    fn agent() -> AgentId {
        AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()
    }

    fn other() -> AgentId {
        AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAY").unwrap()
    }

    #[derive(Clone, Copy, Debug)]
    enum RoleCase {
        Operator,
        AgentA,
        AgentB,
        Unproven,
    }

    impl RoleCase {
        const ALL: [Self; 4] = [Self::Operator, Self::AgentA, Self::AgentB, Self::Unproven];

        fn role(self) -> PeerRole {
            match self {
                Self::Operator => PeerRole::Operator,
                Self::AgentA => PeerRole::Agent { agent: agent() },
                Self::AgentB => PeerRole::Agent { agent: other() },
                Self::Unproven => PeerRole::Unproven,
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum TargetCase {
        Primary,
        OtherAgent,
        WrongKind,
    }

    impl TargetCase {
        const ALL: [Self; 3] = [Self::Primary, Self::OtherAgent, Self::WrongKind];
    }

    #[derive(Clone, Copy, Debug)]
    struct TableCell {
        method: ConditionedMethod,
        provenance: PeerProvenance,
        role: RoleCase,
        target: TargetCase,
        authority: bool,
        owns_pty: bool,
        over_cap: bool,
    }

    impl TableCell {
        fn facts(self, authority: &OperatorEffectAuthority) -> Facts<'_> {
            Facts {
                role: self.role.role(),
                provenance: self.provenance,
                target: target_case(self.method, self.target),
                authority: self.authority.then_some(authority),
                owns_pty: self.owns_pty,
                over_cap: self.over_cap,
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ExpectedDisposition {
        Apply,
        Suppress,
        Quarantine,
    }

    impl From<Disposition> for ExpectedDisposition {
        fn from(disposition: Disposition) -> Self {
            match disposition {
                Disposition::Apply => Self::Apply,
                Disposition::Suppress(_) => Self::Suppress,
                Disposition::Quarantine(_) => Self::Quarantine,
            }
        }
    }

    fn expected_disposition(cell: TableCell) -> ExpectedDisposition {
        let target = target_case(cell.method, cell.target);
        let target_kind_matches = match cell.method {
            ConditionedMethod::AgentSteer
            | ConditionedMethod::AgentAnswer
            | ConditionedMethod::AgentAttach
            | ConditionedMethod::AgentDetach
            | ConditionedMethod::MemoryAppend => !matches!(cell.target, TargetCase::WrongKind),
            ConditionedMethod::PtyWrite | ConditionedMethod::MailSend => {
                matches!(cell.target, TargetCase::Primary)
            }
        };
        if !target_kind_matches {
            return if cell.method == ConditionedMethod::MemoryAppend {
                ExpectedDisposition::Quarantine
            } else {
                ExpectedDisposition::Suppress
            };
        }

        if cell.method != ConditionedMethod::MemoryAppend {
            let bounded_over_cap = cell.over_cap
                && matches!(
                    cell.method,
                    ConditionedMethod::AgentSteer
                        | ConditionedMethod::AgentAnswer
                        | ConditionedMethod::PtyWrite
                        | ConditionedMethod::MailSend
                );
            let operator_apply = matches!(cell.role, RoleCase::Operator)
                && cell.authority
                && cell.provenance == PeerProvenance::NoRecordedAgent
                && !bounded_over_cap
                && (cell.method != ConditionedMethod::PtyWrite || cell.owns_pty);
            return if operator_apply {
                ExpectedDisposition::Apply
            } else {
                ExpectedDisposition::Suppress
            };
        }

        let memory_apply = !cell.over_cap
            && match (cell.role, target) {
                (RoleCase::Operator, TargetFact::Agent(_)) => {
                    cell.authority && cell.provenance == PeerProvenance::NoRecordedAgent
                }
                (RoleCase::AgentA, TargetFact::Agent(target)) => {
                    target == agent() && cell.provenance == PeerProvenance::Agent(agent())
                }
                (RoleCase::AgentB, TargetFact::Agent(target)) => {
                    target == other() && cell.provenance == PeerProvenance::Agent(other())
                }
                (RoleCase::Unproven, TargetFact::Agent(_))
                | (_, TargetFact::Pty | TargetFact::Mail) => false,
            };
        if memory_apply {
            ExpectedDisposition::Apply
        } else {
            ExpectedDisposition::Quarantine
        }
    }

    fn cells() -> Vec<TableCell> {
        let mut cells = Vec::new();
        for method in ConditionedMethod::ALL {
            for provenance in [
                PeerProvenance::Agent(agent()),
                PeerProvenance::NoRecordedAgent,
                PeerProvenance::Unresolvable,
            ] {
                for role in RoleCase::ALL {
                    for target in TargetCase::ALL {
                        for authority in [false, true] {
                            for owns_pty in [false, true] {
                                for over_cap in [false, true] {
                                    cells.push(TableCell {
                                        method,
                                        provenance,
                                        role,
                                        target,
                                        authority,
                                        owns_pty,
                                        over_cap,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        cells
    }

    fn test_authority() -> OperatorEffectAuthority {
        OperatorAuthoritySource::from_replayed_agents(false)
            .acquire(PeerProvenance::NoRecordedAgent)
            .expect("the test source starts in the pre-agent epoch")
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum AcquisitionMutant {
        None,
        IgnoreSeal,
        IgnoreAgentDeny,
    }

    fn acquisition_model(
        sealed: bool,
        provenance: PeerProvenance,
        mutant: AcquisitionMutant,
    ) -> bool {
        let epoch_open = !sealed || mutant == AcquisitionMutant::IgnoreSeal;
        let corroborated = provenance == PeerProvenance::NoRecordedAgent
            || mutant == AcquisitionMutant::IgnoreAgentDeny;
        epoch_open && corroborated
    }

    #[test]
    fn agent_inaccessibility_matrix_kills_ignore_seal_and_ignore_agent_deny() {
        let cases = [
            (false, PeerProvenance::NoRecordedAgent, true),
            (false, PeerProvenance::Agent(agent()), false),
            (false, PeerProvenance::Unresolvable, false),
            (true, PeerProvenance::NoRecordedAgent, false),
            (true, PeerProvenance::Agent(agent()), false),
            (true, PeerProvenance::Unresolvable, false),
        ];
        for (sealed, provenance, expected) in cases {
            assert_eq!(
                acquisition_model(sealed, provenance, AcquisitionMutant::None),
                expected,
                "production acquisition mismatch: sealed={sealed}, provenance={provenance:?}"
            );
        }
        for mutant in [
            AcquisitionMutant::IgnoreSeal,
            AcquisitionMutant::IgnoreAgentDeny,
        ] {
            let caught = cases
                .iter()
                .filter(|(sealed, provenance, expected)| {
                    acquisition_model(*sealed, *provenance, mutant) != *expected
                })
                .count();
            assert!(
                caught > 0,
                "agent-inaccessibility matrix must turn red for {mutant:?}"
            );
        }
    }

    #[test]
    fn source_opens_only_for_zero_agent_replay_and_seals_irreversibly() {
        let source = OperatorAuthoritySource::from_replayed_agents(false);
        assert!(!source.is_sealed());
        assert!(
            source.acquire(PeerProvenance::Agent(agent())).is_none(),
            "an agent-correlated peer is denied even during the open epoch"
        );
        let lease = source
            .acquire(PeerProvenance::NoRecordedAgent)
            .expect("the positive control acquires before any agent is recorded");
        assert_eq!(source.live_leases(), 1);
        source.seal();
        source.seal();
        assert!(source.is_sealed(), "sealing is idempotent and irreversible");
        assert!(
            source.acquire(PeerProvenance::NoRecordedAgent).is_none(),
            "no connection acquires after sealing"
        );
        assert!(lease.is_live(), "an existing connection survives sealing");
        drop(lease);
        assert_eq!(source.live_leases(), 0);

        let replayed = OperatorAuthoritySource::from_replayed_agents(true);
        assert!(replayed.is_sealed());
        assert!(
            replayed.acquire(PeerProvenance::NoRecordedAgent).is_none(),
            "any replayed agent starts the run sealed"
        );
    }

    #[test]
    fn post_close_refusal_kills_ignore_revoked_mutant() {
        use std::sync::{Arc, Mutex};

        let source = OperatorAuthoritySource::from_replayed_agents(false);
        let context = ConditionedConnectionContext::from_accepted_peer(
            PeerIdentity {
                uid: Some(501),
                gid: Some(20),
                pid: PeerPid::Reported {
                    pid: 42,
                    pidversion: Some(7),
                },
            },
            PeerProvenance::NoRecordedAgent,
            Some(&source),
        );
        let effects = Arc::new(Mutex::new(0_u8));
        let effect_calls = Arc::clone(&effects);
        let records = Arc::new(Mutex::new(Vec::new()));
        let durable = Arc::clone(&records);
        let route = route(
            ConditionedMethod::AgentAnswer,
            move |_, _| {
                Ok(DecodedEffect::new(
                    "answer",
                    ConditionedTarget::Agent { agent: agent() },
                    Some(b"answer".to_vec()),
                ))
            },
            move |operation| {
                if let Some(record) = operation.disposition_record() {
                    durable.lock().unwrap().push(record.clone());
                }
                Ok(())
            },
            move |_, ()| {
                *effect_calls.lock().unwrap() += 1;
                Ok(Value::Null)
            },
        );

        route(None, &PeerRole::Operator, &context).expect("the live lease applies");
        assert_eq!(*effects.lock().unwrap(), 1);
        context.revoke();
        assert_eq!(source.live_leases(), 0);

        let refused = route(None, &PeerRole::Operator, &context)
            .expect_err("the same context suppresses after close revocation");
        assert_eq!(refused.code, ErrorCode::Refused.code());
        assert_eq!(*effects.lock().unwrap(), 1, "no second effect ran");
        assert!(matches!(
            records.lock().unwrap().as_slice(),
            [Record::ConditionedEffectSuppressed { .. }]
        ));

        let revoked = context
            .authority
            .as_ref()
            .expect("the revoked lease remains owned only for this mutant check");
        assert_eq!(
            decide(
                ConditionedMethod::AgentAnswer,
                Facts {
                    role: PeerRole::Operator,
                    provenance: PeerProvenance::NoRecordedAgent,
                    target: TargetFact::Agent(agent()),
                    authority: Some(revoked),
                    owns_pty: false,
                    over_cap: false,
                },
            ),
            Disposition::Apply,
            "IgnoreRevoked — passing the raw revoked lease instead of the live-filtered view — \
             turns the post-close assertion red"
        );
    }

    #[test]
    fn detach_waits_for_inflight_write_completion_before_close() {
        use std::sync::Barrier;
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let source = OperatorAuthoritySource::from_replayed_agents(false);
        let context = Arc::new(ConditionedConnectionContext::from_accepted_peer(
            PeerIdentity {
                uid: Some(501),
                gid: Some(20),
                pid: PeerPid::Reported {
                    pid: 42,
                    pidversion: Some(7),
                },
            },
            PeerProvenance::NoRecordedAgent,
            Some(&source),
        ));
        let pty = PtyId(77);
        context
            .bind_attachment(agent(), pty, Arc::new(|_, _| {}))
            .expect("one owned attachment");

        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let write_entered = Arc::clone(&entered);
        let write_release = Arc::clone(&release);
        let write = Arc::new(write_route(
            move |_, _| {
                Ok(DecodedEffect::new(
                    (),
                    ConditionedTarget::Pty { pty },
                    Some(vec![42]),
                ))
            },
            move |operation| {
                assert!(operation.applied_effect().is_some());
                write_entered.wait();
                write_release.wait();
                Ok(dudes_proto::PtyWriteOutcome::Complete { written_len: 1 })
            },
        ));
        let (close_tx, close_rx) = mpsc::channel();
        let detach = Arc::new(detach_route(
            move |_, _| {
                Ok(DecodedEffect::new(
                    (),
                    ConditionedTarget::Agent { agent: agent() },
                    None,
                ))
            },
            move |operation, bound| {
                assert!(operation.applied_effect().is_some());
                assert_eq!(bound, Some(pty));
                close_tx.send(()).expect("close observation");
                Ok(Some(dudes_proto::PtyCloseOutcome::Closed))
            },
        ));
        let write_context = Arc::clone(&context);
        let writer = thread::spawn(move || {
            write(None, &PeerRole::Operator, &write_context).expect("owned write")
        });
        entered.wait();
        let detach_context = Arc::clone(&context);
        let detacher = thread::spawn(move || {
            detach(None, &PeerRole::Operator, &detach_context).expect("confirmed detach")
        });
        assert!(
            close_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "close cannot reach the host while an admitted write is still pending"
        );
        release.wait();
        writer.join().expect("writer thread");
        detacher.join().expect("detach thread");
        close_rx
            .try_recv()
            .expect("one close followed write completion");
        assert!(
            !context.owns_pty(),
            "the binding was revoked before the close returned"
        );
    }

    #[test]
    fn disposition_table_exhausts_every_relevant_dimension() {
        let authority = test_authority();
        let cells = cells();
        assert_eq!(
            cells.len(),
            7 * 3 * 4 * 3 * 2 * 2 * 2,
            "method × provenance × role × target × authority × PTY ownership × over-cap payload"
        );
        for cell in cells {
            assert_eq!(
                ExpectedDisposition::from(decide(cell.method, cell.facts(&authority))),
                expected_disposition(cell),
                "disposition mismatch for {cell:?}"
            );
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Mutant {
        Method,
        Provenance,
        Role,
        Target,
        Authority,
        PtyOwner,
        OverCap,
    }

    #[test]
    fn every_table_dimension_catches_a_deliberately_weakened_decider() {
        let authority = test_authority();
        let cells = cells();
        for mutant in [
            Mutant::Method,
            Mutant::Provenance,
            Mutant::Role,
            Mutant::Target,
            Mutant::Authority,
            Mutant::PtyOwner,
            Mutant::OverCap,
        ] {
            let caught = cells
                .iter()
                .copied()
                .filter(|cell| {
                    let actual = decide_mutant(*cell, mutant, &authority);
                    ExpectedDisposition::from(actual) != expected_disposition(*cell)
                })
                .count();
            assert!(
                caught > 0,
                "positive control: the exhaustive table catches {mutant:?}"
            );
        }
    }

    fn decide_mutant(
        cell: TableCell,
        mutant: Mutant,
        authority: &OperatorEffectAuthority,
    ) -> Disposition {
        let mut method = cell.method;
        let mut facts = cell.facts(authority);
        match mutant {
            Mutant::Method => {
                method = if method == ConditionedMethod::MemoryAppend {
                    ConditionedMethod::AgentSteer
                } else {
                    ConditionedMethod::MemoryAppend
                };
            }
            Mutant::Provenance => facts.provenance = PeerProvenance::NoRecordedAgent,
            Mutant::Role => {
                facts.role = if matches!(facts.role, PeerRole::Operator) {
                    PeerRole::Unproven
                } else {
                    PeerRole::Operator
                };
            }
            Mutant::Target => facts.target = target_for(method),
            Mutant::Authority => facts.authority = Some(authority),
            Mutant::PtyOwner => facts.owns_pty = true,
            // The weakening this mutant exists to catch: a decider that ignores the cap.
            Mutant::OverCap => facts.over_cap = false,
        }
        decide(method, facts)
    }

    fn target_case(method: ConditionedMethod, target: TargetCase) -> TargetFact {
        match target {
            TargetCase::OtherAgent => TargetFact::Agent(other()),
            TargetCase::Primary => match method {
                ConditionedMethod::AgentSteer
                | ConditionedMethod::AgentAnswer
                | ConditionedMethod::AgentAttach
                | ConditionedMethod::AgentDetach
                | ConditionedMethod::MemoryAppend => TargetFact::Agent(agent()),
                ConditionedMethod::PtyWrite => TargetFact::Pty,
                ConditionedMethod::MailSend => TargetFact::Mail,
            },
            TargetCase::WrongKind => match method {
                ConditionedMethod::AgentSteer
                | ConditionedMethod::AgentAnswer
                | ConditionedMethod::AgentAttach
                | ConditionedMethod::AgentDetach
                | ConditionedMethod::MemoryAppend
                | ConditionedMethod::MailSend => TargetFact::Pty,
                ConditionedMethod::PtyWrite => TargetFact::Mail,
            },
        }
    }

    fn target_for(method: ConditionedMethod) -> TargetFact {
        match method {
            ConditionedMethod::AgentSteer
            | ConditionedMethod::AgentAnswer
            | ConditionedMethod::AgentAttach
            | ConditionedMethod::AgentDetach
            | ConditionedMethod::MemoryAppend => TargetFact::Agent(agent()),
            ConditionedMethod::PtyWrite => TargetFact::Pty,
            ConditionedMethod::MailSend => TargetFact::Mail,
        }
    }

    #[test]
    fn router_persists_suppression_and_never_reaches_the_sink() {
        use std::sync::{Arc, Mutex};

        let records = Arc::new(Mutex::new(Vec::new()));
        let persisted = Arc::clone(&records);
        let sink_calls = Arc::new(Mutex::new(0_u8));
        let called = Arc::clone(&sink_calls);
        let route = route(
            ConditionedMethod::AgentAnswer,
            move |_, _| {
                Ok(DecodedEffect::new(
                    "answer",
                    ConditionedTarget::Agent { agent: agent() },
                    Some(b"hello".to_vec()),
                ))
            },
            move |operation| {
                let record = operation
                    .disposition_record()
                    .expect("suppression is a disposition operation");
                persisted.lock().unwrap().push(record.clone());
                Ok(())
            },
            move |_, ()| {
                *called.lock().unwrap() += 1;
                Ok(Value::Null)
            },
        );

        let connection =
            ConditionedConnectionContext::without_peer(PeerProvenance::NoRecordedAgent);
        let err = route(None, &PeerRole::Operator, &connection).unwrap_err();
        assert_eq!(err.code, ErrorCode::Refused.code());
        assert_eq!(
            err.data.unwrap()["gate"],
            serde_json::json!("conditioned_effect")
        );
        assert_eq!(*sink_calls.lock().unwrap(), 0);
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 1);
        let Record::ConditionedEffectSuppressed {
            method,
            provenance,
            payload: Some(payload),
            ..
        } = &records[0]
        else {
            panic!("expected one suppression record")
        };
        assert_eq!(*method, ConditionedMethod::AgentAnswer);
        assert_eq!(*provenance, PeerProvenance::NoRecordedAgent);
        assert_eq!(payload.decoded_len, 5);
        assert_eq!(
            payload.blake3,
            "ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f"
        );
    }

    #[test]
    fn router_creates_one_applied_capability_for_own_agent_memory() {
        use std::sync::{Arc, Mutex};

        let observed = Arc::new(Mutex::new(None));
        let sink_observed = Arc::clone(&observed);
        let persisted_identity = Arc::new(Mutex::new(None));
        let identity = Arc::clone(&persisted_identity);
        let order = Arc::new(Mutex::new(Vec::new()));
        let persist_order = Arc::clone(&order);
        let sink_order = Arc::clone(&order);
        let route = route(
            ConditionedMethod::MemoryAppend,
            move |_, _| {
                Ok(DecodedEffect::new(
                    "memory text",
                    ConditionedTarget::Agent { agent: agent() },
                    Some(b"memory text".to_vec()),
                ))
            },
            move |operation| {
                assert!(operation.disposition_record().is_none());
                let applied = operation
                    .applied_effect()
                    .expect("Apply reaches the applied persistence operation");
                assert_eq!(applied.params(), &"memory text");
                assert_eq!(
                    applied.target(),
                    &ConditionedTarget::Agent { agent: agent() }
                );
                *identity.lock().unwrap() = Some((applied.provenance(), applied.actor()));
                persist_order.lock().unwrap().push("persisted");
                Ok(())
            },
            move |applied, ()| {
                assert_eq!(applied.params(), &"memory text");
                assert_eq!(
                    applied.target(),
                    &ConditionedTarget::Agent { agent: agent() }
                );
                sink_order.lock().unwrap().push("sink");
                *sink_observed.lock().unwrap() = Some(applied.into_params());
                Ok(serde_json::json!({"applied": true}))
            },
        );

        let connection = ConditionedConnectionContext::without_peer(PeerProvenance::Agent(agent()));
        let result = route(None, &PeerRole::Agent { agent: agent() }, &connection).unwrap();
        assert_eq!(result, serde_json::json!({"applied": true}));
        assert_eq!(*observed.lock().unwrap(), Some("memory text"));
        assert_eq!(
            *persisted_identity.lock().unwrap(),
            Some((
                PeerProvenance::Agent(agent()),
                Actor::Agent { agent: agent() }
            ))
        );
        assert_eq!(*order.lock().unwrap(), ["persisted", "sink"]);
    }

    #[test]
    fn router_quarantines_mismatched_memory_without_retaining_bytes() {
        use std::sync::{Arc, Mutex};

        let records = Arc::new(Mutex::new(Vec::new()));
        let persisted = Arc::clone(&records);
        let route = route(
            ConditionedMethod::MemoryAppend,
            move |_, _| {
                Ok(DecodedEffect::new(
                    (),
                    ConditionedTarget::Agent { agent: agent() },
                    Some(b"withheld".to_vec()),
                ))
            },
            move |operation| {
                let record = operation
                    .disposition_record()
                    .expect("quarantine is a disposition operation");
                persisted.lock().unwrap().push(record.clone());
                Ok(())
            },
            move |_, ()| panic!("quarantine must not reach a sink"),
        );

        let connection = ConditionedConnectionContext::without_peer(PeerProvenance::Agent(other()));
        let err = route(None, &PeerRole::Agent { agent: agent() }, &connection).unwrap_err();
        assert_eq!(err.code, ErrorCode::Refused.code());
        let records = records.lock().unwrap();
        let Record::MemoryAppendQuarantined { payload, .. } = &records[0] else {
            panic!("expected quarantine")
        };
        assert_eq!(payload.decoded_len, 8);
        let json = serde_json::to_string(&records[0]).unwrap();
        assert!(
            !json.contains("withheld"),
            "quarantine retains no payload bytes"
        );
    }

    #[derive(Debug)]
    struct PayloadBoundaryLedger<M> {
        dispositions: Mutex<Vec<Record>>,
        persisted_params: Mutex<Vec<M>>,
        persisted_payloads: Mutex<Vec<Vec<u8>>>,
        sink_params: Mutex<Vec<M>>,
    }

    impl<M> Default for PayloadBoundaryLedger<M> {
        fn default() -> Self {
            Self {
                dispositions: Mutex::new(Vec::new()),
                persisted_params: Mutex::new(Vec::new()),
                persisted_payloads: Mutex::new(Vec::new()),
                sink_params: Mutex::new(Vec::new()),
            }
        }
    }

    fn route_payload<M>(
        method: ConditionedMethod,
        params: M,
        target: ConditionedTarget,
        payload: Vec<u8>,
        owns_pty: bool,
    ) -> (Result<Value, RpcError>, Arc<PayloadBoundaryLedger<M>>)
    where
        M: Clone + Send + Sync + 'static,
    {
        let ledger = Arc::new(PayloadBoundaryLedger::default());
        let persisted = Arc::clone(&ledger);
        let sank = Arc::clone(&ledger);
        let route = route(
            method,
            move |_, _| {
                Ok(DecodedEffect::new(
                    params.clone(),
                    target.clone(),
                    Some(payload.clone()),
                ))
            },
            move |operation| {
                if let Some(record) = operation.disposition_record() {
                    persisted
                        .dispositions
                        .lock()
                        .expect("the boundary ledger lock")
                        .push(record.clone());
                } else {
                    let applied = operation
                        .applied_effect()
                        .expect("a persistence operation has exactly one arm");
                    persisted
                        .persisted_params
                        .lock()
                        .expect("the boundary ledger lock")
                        .push(applied.params().clone());
                    persisted
                        .persisted_payloads
                        .lock()
                        .expect("the boundary ledger lock")
                        .push(
                            applied
                                .payload()
                                .expect("this method has a payload")
                                .to_vec(),
                        );
                }
                Ok(())
            },
            move |applied, ()| {
                sank.sink_params
                    .lock()
                    .expect("the boundary ledger lock")
                    .push(applied.into_params());
                Ok(Value::Null)
            },
        );
        let source = OperatorAuthoritySource::from_replayed_agents(false);
        let mut connection = ConditionedConnectionContext::from_accepted_peer(
            PeerIdentity {
                uid: Some(501),
                gid: Some(20),
                pid: PeerPid::Reported {
                    pid: 42,
                    pidversion: Some(7),
                },
            },
            PeerProvenance::NoRecordedAgent,
            Some(&source),
        );
        connection.owns_pty = owns_pty;
        let result = route(None, &PeerRole::Operator, &connection);
        (result, ledger)
    }

    fn assert_conditioned_refusal(error: &RpcError) {
        assert_eq!(error.code, ErrorCode::Refused.code());
        assert_eq!(
            error.data.as_ref().expect("typed refusal data")["gate"],
            serde_json::json!("conditioned_effect")
        );
    }

    struct PayloadBoundaryCase<M> {
        method: ConditionedMethod,
        cap: usize,
        at_cap_params: M,
        over_cap_params: M,
        target: ConditionedTarget,
        at_cap_payload: Vec<u8>,
        over_cap_payload: Vec<u8>,
        owns_pty: bool,
    }

    fn assert_payload_ceiling<M>(case: PayloadBoundaryCase<M>)
    where
        M: Clone + std::fmt::Debug + PartialEq + Send + Sync + 'static,
    {
        let PayloadBoundaryCase {
            method,
            cap,
            at_cap_params,
            over_cap_params,
            target,
            at_cap_payload,
            over_cap_payload,
            owns_pty,
        } = case;
        let over_digest = payload_digest(&over_cap_payload);
        let (result, ledger) = route_payload(
            method,
            over_cap_params,
            target.clone(),
            over_cap_payload,
            owns_pty,
        );
        let error = result.expect_err("one byte over the method ceiling is refused");
        assert_conditioned_refusal(&error);
        let dispositions = ledger
            .dispositions
            .lock()
            .expect("the boundary ledger lock");
        assert_eq!(dispositions.len(), 1, "one durable suppression");
        assert!(matches!(
            &dispositions[0],
            Record::ConditionedEffectSuppressed {
                method: actual_method,
                target: actual_target,
                reason,
                payload: Some(payload),
                ..
            } if *actual_method == method
                && actual_target == &target
                && reason == "decoded payload exceeds the method ceiling"
                && payload == &over_digest
                && payload.decoded_len == u64::try_from(cap + 1).expect("length fits u64")
        ));
        assert!(
            ledger
                .sink_params
                .lock()
                .expect("the boundary ledger lock")
                .is_empty()
        );
        drop(dispositions);

        let (result, ledger) = route_payload(
            method,
            at_cap_params.clone(),
            target,
            at_cap_payload.clone(),
            owns_pty,
        );
        assert_eq!(result.expect("exactly at the ceiling applies"), Value::Null);
        assert!(
            ledger
                .dispositions
                .lock()
                .expect("the boundary ledger lock")
                .is_empty()
        );
        assert_eq!(
            ledger
                .persisted_params
                .lock()
                .expect("the boundary ledger lock")
                .as_slice(),
            std::slice::from_ref(&at_cap_params)
        );
        assert_eq!(
            *ledger
                .persisted_payloads
                .lock()
                .expect("the boundary ledger lock"),
            [at_cap_payload],
            "persistence observes the complete decoded payload"
        );
        assert_eq!(
            *ledger.sink_params.lock().expect("the boundary ledger lock"),
            [at_cap_params],
            "the sink observes complete untruncated params"
        );
    }

    #[test]
    fn agent_steer_over_cap_suppresses_once_and_exact_cap_applies() {
        let cap = MAX_AGENT_STEER_TEXT_BYTES;
        assert_payload_ceiling(PayloadBoundaryCase {
            method: ConditionedMethod::AgentSteer,
            cap,
            at_cap_params: dudes_proto::rpc::AgentText {
                agent: agent(),
                text: "s".repeat(cap),
            },
            over_cap_params: dudes_proto::rpc::AgentText {
                agent: agent(),
                text: "s".repeat(cap + 1),
            },
            target: ConditionedTarget::Agent { agent: agent() },
            at_cap_payload: vec![b's'; cap],
            over_cap_payload: vec![b's'; cap + 1],
            owns_pty: false,
        });
    }

    #[test]
    fn agent_answer_over_cap_suppresses_once_and_exact_cap_applies() {
        let cap = MAX_AGENT_ANSWER_TEXT_BYTES;
        assert_payload_ceiling(PayloadBoundaryCase {
            method: ConditionedMethod::AgentAnswer,
            cap,
            at_cap_params: dudes_proto::rpc::AgentText {
                agent: agent(),
                text: "a".repeat(cap),
            },
            over_cap_params: dudes_proto::rpc::AgentText {
                agent: agent(),
                text: "a".repeat(cap + 1),
            },
            target: ConditionedTarget::Agent { agent: agent() },
            at_cap_payload: vec![b'a'; cap],
            over_cap_payload: vec![b'a'; cap + 1],
            owns_pty: false,
        });
    }

    #[test]
    fn mail_send_over_cap_suppresses_once_and_exact_cap_selects_apply() {
        use dudes_proto::MailAct;
        use dudes_proto::rpc::mail::MailSendParams;

        let cap = MAX_MAIL_SEND_BODY_BYTES;
        let to = dudes_proto::MailTo::Agent { agent: agent() };
        assert_payload_ceiling(PayloadBoundaryCase {
            method: ConditionedMethod::MailSend,
            cap,
            at_cap_params: MailSendParams {
                to,
                act: MailAct::Message,
                subject: "bounded".into(),
                body: "m".repeat(cap),
            },
            over_cap_params: MailSendParams {
                to,
                act: MailAct::Message,
                subject: "bounded".into(),
                body: "m".repeat(cap + 1),
            },
            target: ConditionedTarget::Mail { to },
            at_cap_payload: vec![b'm'; cap],
            over_cap_payload: vec![b'm'; cap + 1],
            owns_pty: false,
        });
    }

    fn zero_bytes_base64(decoded_len: usize) -> String {
        let mut encoded = "AAAA".repeat(decoded_len / 3);
        match decoded_len % 3 {
            0 => {}
            1 => encoded.push_str("AA=="),
            2 => encoded.push_str("AAA="),
            _ => unreachable!("the remainder is less than three"),
        }
        encoded
    }

    #[test]
    fn pty_write_over_cap_suppresses_once_and_exact_cap_selects_apply() {
        use dudes_proto::rpc::pty::{PtyId, PtyWriteParams};

        let cap = MAX_PTY_WRITE_BYTES;
        let pty = PtyId(7);
        assert_payload_ceiling(PayloadBoundaryCase {
            method: ConditionedMethod::PtyWrite,
            cap,
            at_cap_params: PtyWriteParams {
                pty,
                data: zero_bytes_base64(cap),
            },
            over_cap_params: PtyWriteParams {
                pty,
                data: zero_bytes_base64(cap + 1),
            },
            target: ConditionedTarget::Pty { pty },
            at_cap_payload: vec![0; cap],
            over_cap_payload: vec![0; cap + 1],
            owns_pty: true,
        });
    }

    #[test]
    fn every_conditioned_method_maps_to_its_independent_payload_ceiling() {
        assert_eq!(
            payload_ceiling(ConditionedMethod::AgentSteer),
            Some(MAX_AGENT_STEER_TEXT_BYTES)
        );
        assert_eq!(
            payload_ceiling(ConditionedMethod::AgentAnswer),
            Some(MAX_AGENT_ANSWER_TEXT_BYTES)
        );
        assert_eq!(
            payload_ceiling(ConditionedMethod::MailSend),
            Some(MAX_MAIL_SEND_BODY_BYTES)
        );
        assert_eq!(
            payload_ceiling(ConditionedMethod::PtyWrite),
            Some(MAX_PTY_WRITE_BYTES)
        );
        assert_eq!(
            payload_ceiling(ConditionedMethod::MemoryAppend),
            Some(MAX_MEMORY_APPEND_TEXT_BYTES)
        );
        assert_eq!(payload_ceiling(ConditionedMethod::AgentAttach), None);
        assert_eq!(payload_ceiling(ConditionedMethod::AgentDetach), None);
    }

    fn assert_request_frame_fits(
        method: Method,
        params: Value,
        payload_field: &str,
        expected_wire_len: usize,
    ) {
        let request = dudes_proto::rpc::Request {
            jsonrpc: dudes_proto::rpc::JSONRPC.to_owned(),
            id: dudes_proto::rpc::RequestId::Number(1),
            method: method.name().to_owned(),
            params: Some(params),
            v: None,
            cred: None,
        };
        let mut frame = Vec::new();
        crate::codec::write_frame(&mut frame, &request).expect("the exact-cap request frame fits");
        assert!(frame.len() <= crate::codec::MAX_FRAME_BYTES);
        let decoded: dudes_proto::rpc::Request =
            serde_json::from_slice(frame.strip_suffix(b"\n").expect("one terminated frame"))
                .expect("the written frame decodes");
        assert_eq!(
            decoded.params.expect("params")[payload_field]
                .as_str()
                .expect("a string payload")
                .len(),
            expected_wire_len,
            "the exact-cap wire payload was not truncated"
        );
    }

    #[test]
    fn exact_cap_request_frames_fit_with_worst_text_escaping_and_base64() {
        let escaped_answer = "\u{1}".repeat(MAX_AGENT_ANSWER_TEXT_BYTES);
        assert_request_frame_fits(
            Method::AgentAnswer,
            serde_json::json!({ "agent": agent(), "text": escaped_answer }),
            "text",
            MAX_AGENT_ANSWER_TEXT_BYTES,
        );
        let escaped_steer = "\u{1}".repeat(MAX_AGENT_STEER_TEXT_BYTES);
        assert_request_frame_fits(
            Method::AgentSteer,
            serde_json::json!({ "agent": agent(), "text": escaped_steer }),
            "text",
            MAX_AGENT_STEER_TEXT_BYTES,
        );
        let escaped_mail = "\u{1}".repeat(MAX_MAIL_SEND_BODY_BYTES);
        assert_request_frame_fits(
            Method::MailSend,
            serde_json::json!({
                "to": { "kind": "agent", "agent": agent() },
                "act": "message",
                "subject": "bounded",
                "body": escaped_mail,
            }),
            "body",
            MAX_MAIL_SEND_BODY_BYTES,
        );
        let encoded = zero_bytes_base64(MAX_PTY_WRITE_BYTES);
        assert_request_frame_fits(
            Method::PtyWrite,
            serde_json::json!({ "pty": 7, "data": encoded }),
            "data",
            4 * MAX_PTY_WRITE_BYTES.div_ceil(3),
        );
    }

    #[test]
    fn invalid_conditioned_params_stop_before_disposition_and_sink() {
        let persists = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sinks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let persist_calls = Arc::clone(&persists);
        let sink_calls = Arc::clone(&sinks);
        let route = route::<(), (), _, _, _>(
            ConditionedMethod::MailSend,
            |_, _| Err(invalid_params("malformed mail params")),
            move |_| {
                persist_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            move |_, ()| {
                sink_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Value::Null)
            },
        );
        let context = ConditionedConnectionContext::without_peer(PeerProvenance::NoRecordedAgent);
        let error =
            route(None, &PeerRole::Operator, &context).expect_err("invalid params are refused");
        assert_eq!(error.code, ErrorCode::InvalidParams.code());
        assert_eq!(persists.load(Ordering::SeqCst), 0);
        assert_eq!(sinks.load(Ordering::SeqCst), 0);
    }

    /// Invalid params, placement: a U+0000 answer stops in the decoder the registration
    /// installs for `agent.answer`, before the disposition. On a connection without effect
    /// authority it gets `-32602`, and no suppression is persisted for bytes the method can never
    /// apply.
    ///
    /// Controls: the same call with U+0001 in its place, and the same U+0000 on `agent.steer`,
    /// which keeps accepting U+0000, each persist exactly one disposition — the suppression.
    ///
    /// Mutation: move the check behind the disposition, and the U+0000 answer persists one.
    #[test]
    fn a_nul_answer_stops_at_decode_before_any_disposition() {
        use dudes_proto::rpc::cred::OperatorCredential;
        use dudes_proto::rpc::{AgentText, JSONRPC, Request, RequestId};
        use std::sync::atomic::AtomicUsize;

        #[derive(Default)]
        struct Persisted {
            answers: AtomicUsize,
            steers: AtomicUsize,
        }
        impl crate::methods::ConditionedAgentControl for Persisted {
            fn answer(&self, _: PersistenceOperation<'_, AgentText>) -> Result<(), RpcError> {
                self.answers.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn steer(&self, _: PersistenceOperation<'_, AgentText>) -> Result<(), RpcError> {
                self.steers.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let credential = || OperatorCredential::new("C".repeat(43)).expect("a credential");
        let persisted = Arc::new(Persisted::default());
        let mut dispatcher = crate::Dispatcher::new(crate::PeerClassifier::new(
            credential(),
            Arc::new(crate::testing::FakeMintedTokens::none()),
        ));
        crate::methods::register_agent_effects(&mut dispatcher, &persisted);
        let call = |method: Method, text: &str| {
            dispatcher
                .dispatch(
                    &Request {
                        jsonrpc: JSONRPC.to_owned(),
                        id: RequestId::Number(1),
                        method: method.name().to_owned(),
                        params: Some(serde_json::json!({ "agent": agent(), "text": text })),
                        v: None,
                        cred: Some(credential()),
                    },
                    PeerProvenance::NoRecordedAgent,
                    &Arc::new(crate::Outbound::sink()),
                )
                .error
                .expect("no call on this connection applies")
        };

        let error = call(Method::AgentAnswer, "ma\u{0}in");
        assert_eq!(
            persisted.answers.load(Ordering::SeqCst),
            0,
            "decode stops before any disposition: {error:?}"
        );
        assert_eq!(error.code, ErrorCode::InvalidParams.code(), "{error:?}");
        assert_eq!(error.data, None, "invalid params carry no data");
        assert!(
            error.message.starts_with("agent.answer:") && error.message.contains("byte offset 2"),
            "the message names the method and the first offset: {}",
            error.message
        );

        assert_conditioned_refusal(&call(Method::AgentAnswer, "ma\u{1}in"));
        assert_eq!(
            persisted.answers.load(Ordering::SeqCst),
            1,
            "control: without U+0000 the suppression is persisted"
        );
        assert_conditioned_refusal(&call(Method::AgentSteer, "ma\u{0}in"));
        assert_eq!(
            persisted.steers.load(Ordering::SeqCst),
            1,
            "control: agent.steer reaches its disposition unchanged"
        );
    }

    /// A writer whose bytes this file can read back, shared with the [`Outbound`] built over it.
    ///
    /// Deliberately raw bytes rather than parsed frames: what is measured here is the length
    /// `write_frame` compared against the cap, and that length includes the terminator.
    #[derive(Clone, Debug, Default)]
    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("the sink's lock")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// An applied `memory.append` of `text`, in the **inline** arm.
    ///
    /// # The fixture is synthetic in one way, and that way is the point
    ///
    /// The journal puts a body over 4 KiB out of line: at the cap the daemon writes
    /// `text: ""` plus a `body: PayloadRef`, so no producer emits this record at this length
    /// (`crates/dudes-proto/src/record.rs:472-479`). What licenses measuring a shape no producer
    /// emits is what the cap is *for* — it bounds the largest envelope the daemon could compose
    /// around an admitted text, and the inline arm is that largest envelope. The referenced arm is
    /// strictly smaller: a path and a digest instead of the bytes. Bounding the inline arm bounds
    /// both, which is the conservative direction; bounding the referenced arm would prove nothing
    /// about the bytes at all.
    fn appended_memory(text: String) -> Vec<u8> {
        let entry = dudes_proto::record::Entry {
            at: dudes_types::Timestamp::from_unix_millis(1_787_394_600_000),
            record: Record::MemoryAppend {
                agent: agent(),
                text,
                body: None,
                provenance: Some(PeerProvenance::Agent(agent())),
            },
        };
        let sink = Sink::default();
        let out = std::sync::Arc::new(crate::outbound::Outbound::new(sink.clone()));
        let subscribers = crate::tail::Subscribers::new();
        subscribers
            .subscribe(&out, || Ok(Vec::new()))
            .expect("an empty backfill subscribes");
        subscribers.appended(dudes_types::Lsn(1), &entry);
        let locked = sink.0.lock().expect("the sink's lock");
        locked.clone()
    }

    /// A text at the cap still composes one deliverable `journal.appended` frame — in ASCII and in
    /// the worst byte pattern the cap admits — which is the derivation's own claim, measured.
    ///
    /// # What this covers that the disposition test cannot
    ///
    /// [`MAX_MEMORY_APPEND_TEXT_BYTES`] exists because the daemon re-wraps an admitted text into a
    /// notification the journal writer preflights against [`MAX_FRAME_BYTES`]
    /// (`crates/dudes-journal/src/writer.rs:185`), and a refusal *there* is after admission. Every
    /// other test of this cap drives the router, which never composes that frame. Without this
    /// one, the derivation's margin was prose: the constant's docs claimed an envelope would have
    /// to grow fifteenfold, and the real figure is 2.67×.
    ///
    /// # Controls
    ///
    /// - The **lower** bounds. A frame that fits proves nothing if it is not the frame that was
    ///   composed: an empty sink, a dropped `text` and a truncated one all satisfy an upper bound
    ///   on their own. `> MAX_MEMORY_APPEND_TEXT_BYTES` and `> 6 *` refuse them.
    /// - The **decode**. A byte count cannot tell a `MemoryAppend` from anything else of the same
    ///   size, nor see a truncation smaller than its own slack, so the frame is read back and the
    ///   record's shape and text length asserted.
    /// - The **margin ratchet**. `escaped.len() * 3 > MAX_FRAME_BYTES` fails the day someone
    ///   re-derives the cap on the sixteenfold headroom the ratio suggests rather than the
    ///   under-threefold headroom that is there. It survives a proportional change to
    ///   [`MAX_FRAME_BYTES`], because the cap is a sixteenth of it either way.
    /// - The **negative** control: lengthen the text alone and the identical composition delivers
    ///   zero bytes. Without it, a fan-out that had silently stopped writing would pass every
    ///   assertion above by way of a sink nobody proved could fill.
    ///
    /// [`MAX_FRAME_BYTES`]: crate::codec::MAX_FRAME_BYTES
    #[test]
    fn a_memory_at_the_cap_fits_one_journal_appended_frame() {
        use crate::codec::MAX_FRAME_BYTES;

        let frame = appended_memory("m".repeat(MAX_MEMORY_APPEND_TEXT_BYTES));
        assert!(
            frame.len() <= MAX_FRAME_BYTES,
            "an ASCII text at the cap composes a deliverable frame: {} bytes against a \
             {MAX_FRAME_BYTES}-byte cap",
            frame.len()
        );
        assert!(
            frame.len() > MAX_MEMORY_APPEND_TEXT_BYTES,
            "the load-bearing bound: the frame that fitted carries the text — `0 bytes` here means \
             write_frame refused the envelope, not that the sink broke: {} bytes",
            frame.len()
        );

        let line = frame
            .strip_suffix(b"\n")
            .expect("a frame is a line, and its terminator counts against the cap");
        assert!(
            !line.contains(&b'\n'),
            "one append, one frame: the bytes measured above are that frame and not several"
        );
        let value: Value = serde_json::from_slice(line).expect("the frame is one JSON object");
        assert_eq!(
            value["method"],
            Value::from(dudes_proto::rpc::NotificationKind::JournalAppended.name())
        );
        let params: dudes_proto::rpc::journal::JournalAppendedParams =
            serde_json::from_value(value["params"].clone()).expect("the contract's params");
        let Record::MemoryAppend { text, .. } = params.entry.record else {
            panic!("the fixture composes a MemoryAppend")
        };
        assert_eq!(
            text.len(),
            MAX_MEMORY_APPEND_TEXT_BYTES,
            "and the text arrived at the cap, whole: it is refused rather than truncated"
        );

        // The second arm: the same composition over the worst byte pattern the cap admits. The
        // guard counts bytes and validates no content, so `U+0001` — one byte in, six out as
        // `\u0001` — is as far as an admitted text can expand.
        let escaped = appended_memory("\u{1}".repeat(MAX_MEMORY_APPEND_TEXT_BYTES));
        assert!(
            escaped.len() > 6 * MAX_MEMORY_APPEND_TEXT_BYTES,
            "the expansion is real and is being measured: six bytes out per byte in — {} bytes",
            escaped.len()
        );
        assert!(
            escaped.len() <= MAX_FRAME_BYTES,
            "and six cap-lengths of escapes still fit one frame: {} bytes",
            escaped.len()
        );
        // The margin above is arithmetic — six times the text, plus the envelope — and that
        // arithmetic assumes the envelope costs the same whatever the text contains. Assert the
        // assumption rather than the conclusion: a ratchet on `escaped.len()` cannot fail, because
        // the `6 * MAX_MEMORY_APPEND_TEXT_BYTES` bound six lines up is `0.375 * MAX_FRAME_BYTES`
        // against any such ratchet's `0.333`, and the cap's equality to `MAX_FRAME_BYTES / 16`
        // makes that domination hold for every frame cap. The overhead is the number a
        // record-shape change actually moves, and this bound has no slack: both arms measured 228
        // bytes, so a single content-dependent byte anywhere in the envelope turns it red.
        assert_eq!(
            escaped.len() - 6 * MAX_MEMORY_APPEND_TEXT_BYTES,
            frame.len() - MAX_MEMORY_APPEND_TEXT_BYTES,
            "the journal.appended envelope must cost the same whatever the text contains: a field \
             derived from the text makes the frame content-dependent, and the margin stated above \
             this test stops being six times the text plus a constant"
        );
        assert!(
            escaped.len() > frame.len(),
            "control: the two arms differ only in the text's bytes, so an escape-expanding text \
             must compose a strictly larger frame than an ASCII one — {} vs {}",
            escaped.len(),
            frame.len()
        );

        let over = appended_memory("m".repeat(MAX_FRAME_BYTES));
        assert!(
            over.is_empty(),
            "control on this fixture's sink: lengthening the text alone stops the frame being \
             deliverable, so the fits above are about length and not about a sink that never \
             fills — {} bytes",
            over.len()
        );
    }

    #[test]
    fn connection_context_derives_the_peer_actor_for_persistence_only() {
        let context = ConditionedConnectionContext::from_peer(
            PeerIdentity {
                uid: Some(501),
                gid: Some(20),
                pid: PeerPid::Reported {
                    pid: 42,
                    pidversion: Some(7),
                },
            },
            PeerProvenance::NoRecordedAgent,
        );
        let applied = AppliedEffect {
            params: "answer",
            target: ConditionedTarget::Agent { agent: agent() },
            payload: None,
            payload_digest: None,
            provenance: context.provenance(),
            actor: context.actor_for(&PeerRole::Operator).unwrap(),
        };
        let persistence = AppliedPersistence { effect: &applied };
        assert_eq!(persistence.provenance(), PeerProvenance::NoRecordedAgent);
        assert_eq!(
            persistence.actor(),
            Actor::Peer {
                pid: 42,
                uid: Some(501)
            }
        );
        assert_eq!(applied.params(), &"answer");

        let unknown = ConditionedConnectionContext::from_peer(
            PeerIdentity::unknown(),
            PeerProvenance::Unresolvable,
        );
        assert!(unknown.actor_for(&PeerRole::Operator).is_none());
        assert_eq!(
            unknown
                .actor_for(&PeerRole::Agent { agent: agent() })
                .unwrap()
                .record_actor(),
            Actor::Agent { agent: agent() }
        );
    }
}

#[cfg(test)]
mod closing_transport_admission {
    use super::*;
    use crate::auth::{Authorizer, Presented, Refusal};
    use dudes_proto::rpc::{JSONRPC, Request, RequestId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct Counted(Arc<AtomicUsize>);
    impl Authorizer for Counted {
        fn authorize(&self, _: Presented, _: PeerProvenance, _: &str) -> Result<PeerRole, Refusal> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(PeerRole::Operator)
        }
    }

    #[test]
    fn actual_pidless_transport_is_refused_before_closing_authorization_or_handler() {
        let authorizations = Arc::new(AtomicUsize::new(0));
        let handlers = Arc::new(AtomicUsize::new(0));
        let mut dispatcher = crate::Dispatcher::new(Counted(Arc::clone(&authorizations)));
        let observed = Arc::clone(&handlers);
        dispatcher.register(
            Method::SystemClosingTime,
            crate::Requirement::OperatorOrAgent,
            move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::json!({}))
            },
        );
        let request = Request {
            jsonrpc: JSONRPC.into(),
            id: RequestId::Number(1),
            method: "system.closingTime".into(),
            params: None,
            v: None,
            cred: None,
        };
        let outbound = Arc::new(crate::Outbound::sink());
        let missing = ConditionedConnectionContext::from_peer(
            PeerIdentity::unknown(),
            PeerProvenance::Unresolvable,
        );
        let response = dispatcher.dispatch_on_connection(&request, &missing, &outbound);
        assert_eq!(response.error.map(|error| error.code), Some(-32001));
        assert_eq!(authorizations.load(Ordering::SeqCst), 0);
        assert_eq!(handlers.load(Ordering::SeqCst), 0);

        let known = ConditionedConnectionContext::from_peer(
            PeerIdentity {
                uid: None,
                gid: None,
                pid: crate::auth::PeerPid::Reported {
                    pid: 4242,
                    pidversion: None,
                },
            },
            PeerProvenance::Unresolvable,
        );
        let response = dispatcher.dispatch_on_connection(&request, &known, &outbound);
        assert_eq!(response.result, Some(serde_json::json!({})));
        assert!(response.error.is_none());
        assert_eq!(authorizations.load(Ordering::SeqCst), 1);
        assert_eq!(handlers.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod transport_admission_tests {
    use super::*;
    use crate::auth::{Authorizer, PeerClassifier, PeerPid, Presented, Refusal};
    use dudes_proto::rpc::cred::OperatorCredential;
    use dudes_proto::rpc::memory::MemoryReadResult;
    use dudes_proto::rpc::{JSONRPC, Request, RequestId};
    use dudes_types::AgentId;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct Authorizations {
        classifier: PeerClassifier,
        calls: Arc<AtomicUsize>,
    }
    impl Authorizer for Authorizations {
        fn authorize(
            &self,
            presented: Presented,
            provenance: PeerProvenance,
            method: &str,
        ) -> Result<PeerRole, Refusal> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.classifier.authorize(presented, provenance, method)
        }
    }
    struct Reads(Arc<AtomicUsize>);
    impl crate::methods::MemoryRead for Reads {
        fn read(
            &self,
            _: AgentId,
            _: Option<AgentId>,
            _: Option<Lsn>,
        ) -> Result<MemoryReadResult, RpcError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(MemoryReadResult {
                text: "remember".into(),
                lsn: Lsn(7),
            })
        }
    }

    #[test]
    fn actual_connection_factories_refuse_missing_pid_before_authorization_or_memory() {
        let agent = AgentId::NIL;
        let credential = || OperatorCredential::new("C".repeat(43)).unwrap();
        let token = "t".repeat(43);
        let authorizations = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let mut dispatcher = crate::Dispatcher::new(Authorizations {
            classifier: PeerClassifier::new(
                credential(),
                Arc::new(crate::testing::FakeMintedTokens::minted(&[(&token, agent)])),
            ),
            calls: Arc::clone(&authorizations),
        });
        crate::methods::register_memory_read(&mut dispatcher, &Arc::new(Reads(Arc::clone(&reads))));
        let outbound = Arc::new(crate::Outbound::sink());
        let unknown = PeerIdentity::unknown();
        for context in [
            ConditionedConnectionContext::from_peer(unknown, PeerProvenance::Unresolvable),
            ConditionedConnectionContext::from_accepted_peer(
                unknown,
                PeerProvenance::Unresolvable,
                None,
            ),
        ] {
            for (params, cred) in [
                (serde_json::json!({"agent":agent}), Some(credential())),
                (serde_json::json!({"token":token}), None),
            ] {
                let request = Request {
                    jsonrpc: JSONRPC.into(),
                    id: RequestId::Number(1),
                    method: "memory.read".into(),
                    params: Some(params),
                    v: None,
                    cred,
                };
                let response = dispatcher.dispatch_on_connection(&request, &context, &outbound);
                assert_eq!(response.error.unwrap().code, -32001);
                assert!(response.result.is_none());
            }
        }
        assert_eq!(authorizations.load(Ordering::SeqCst), 0);
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        let known = PeerIdentity {
            uid: None,
            gid: None,
            pid: PeerPid::Reported {
                pid: 4242,
                pidversion: None,
            },
        };
        let context = ConditionedConnectionContext::from_accepted_peer(
            known,
            PeerProvenance::Unresolvable,
            None,
        );
        let request = Request {
            jsonrpc: JSONRPC.into(),
            id: RequestId::Number(1),
            method: "memory.read".into(),
            params: Some(serde_json::json!({"agent":agent})),
            v: None,
            cred: Some(credential()),
        };
        let response = dispatcher.dispatch_on_connection(&request, &context, &outbound);
        assert_eq!(
            response.result,
            Some(serde_json::json!({"text":"remember","lsn":7}))
        );
        assert!(response.error.is_none());
        assert_eq!(authorizations.load(Ordering::SeqCst), 1);
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        dispatcher.register(Method::SystemPing, crate::Requirement::AnyRole, |_call| {
            Ok(serde_json::json!({"version":"test","socket":"test.sock","lsn":0}))
        });
        let request = Request {
            jsonrpc: JSONRPC.into(),
            id: RequestId::Number(2),
            method: "system.ping".into(),
            params: None,
            v: None,
            cred: None,
        };
        let context =
            ConditionedConnectionContext::from_peer(unknown, PeerProvenance::Unresolvable);
        let response = dispatcher.dispatch_on_connection(&request, &context, &outbound);
        assert_eq!(
            response.result,
            Some(serde_json::json!({"version":"test","socket":"test.sock","lsn":0}))
        );
        assert_eq!(authorizations.load(Ordering::SeqCst), 2);
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn production_server_factories_never_mark_transport_pid_not_applicable() {
        let production = include_str!("server.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!production.contains("ConditionedConnectionContext::without_peer"));
        assert_eq!(
            production
                .matches("ConditionedConnectionContext::from_peer(")
                .count(),
            2
        );
        assert_eq!(
            production
                .matches("ConditionedConnectionContext::from_accepted_peer(")
                .count(),
            1
        );
        assert!(
            !ConditionedConnectionContext::without_peer(PeerProvenance::Unresolvable)
                .transport_pid_is_unreadable()
        );
        assert!(
            ConditionedConnectionContext::from_peer(
                PeerIdentity::unknown(),
                PeerProvenance::Unresolvable
            )
            .transport_pid_is_unreadable()
        );
    }
}
