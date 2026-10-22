//! The Floor's browser terminal.

use std::convert::Infallible;
use std::fmt;
use std::io::{self, Read as _};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use dudes_proto::rpc::pty::PtyId;
use dudes_proto::{PayloadDigest, PtyCloseOutcome, PtyWriteOutcome};
use dudes_rpc::methods::pty::decode_terminal_input;
use dudes_types::AgentId;
use serde::{Deserialize, Serialize};

use crate::Handle;
use crate::floor_gateway::{ACCEPT_POLL, stop_requested, write_bytes};
use crate::floor_session::{ChannelKey, IdleClock, TerminalSession};
use crate::floor_websocket::{
    Decoder, Message, base64, close_frame, policy_close_frame, pong_frame, text_frame,
};
use crate::ports::{Answered, Ask};
use crate::pty_output::{MAX_FRAME_BYTES, Output, OutputSubscription};
use crate::runtime::terminal_size_in_bounds;

pub(crate) const TERMINAL_PROTOCOL: &str = "dudes-terminal-v1";

/// A silent channel's recheck is bounded at two seconds. The margin covers one
/// [`ACCEPT_POLL`] and a busy scheduler.
const RECHECK_AFTER: Duration = Duration::from_millis(1_500);

const MAX_INPUT_BYTES: usize = 65_536;

/// The bound on one terminal channel's queued frames, counted on the wire.
const WIRE_BOUND_BYTES: usize = 8 * 1024 * 1024;

/// A server frame over 65,535 bytes has a 2-byte header and an 8-byte length.
const LONG_FRAME_HEADER_BYTES: usize = 10;
const DATA_PREFIX: &str = r#"{"type":"terminal.data","data":""#;
const DATA_SUFFIX: &str = r#""}"#;
const FULL_DATA_FRAME_BYTES: usize = LONG_FRAME_HEADER_BYTES
    + DATA_PREFIX.len()
    + MAX_FRAME_BYTES.div_ceil(3) * 4
    + DATA_SUFFIX.len();

/// The PTY output one terminal channel may queue: as many full frames as fit the 8 MiB
/// on the wire.
pub const TERMINAL_QUEUE_BYTES: usize = WIRE_BOUND_BYTES / FULL_DATA_FRAME_BYTES * MAX_FRAME_BYTES;

const _: () = assert!(TERMINAL_QUEUE_BYTES.div_ceil(MAX_FRAME_BYTES) <= 256);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TerminalView {}

pub(crate) const TERMINAL_VIEW: TerminalView = TerminalView {};

#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum ClientMessage {
    #[serde(rename = "terminal.attach")]
    Attach {
        agent: AgentId,
        cols: u16,
        rows: u16,
    },
    #[serde(rename = "terminal.write")]
    Write { handle: String, data: String },
    #[serde(rename = "terminal.resize")]
    Resize {
        handle: String,
        cols: u16,
        rows: u16,
    },
    #[serde(rename = "terminal.detach")]
    Detach { handle: String },
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ServerMessage<'a> {
    #[serde(rename = "terminal.attached")]
    Attached { handle: &'a str },
    #[serde(rename = "terminal.detached")]
    Detached,
    #[serde(rename = "terminal.closed")]
    Closed,
}

fn message_frame(message: &ServerMessage<'_>) -> Vec<u8> {
    text_frame(&serde_json::to_vec(message).expect("a terminal message serializes"))
        .expect("a terminal message fits one frame")
}

fn data_frame(bytes: &[u8]) -> Vec<u8> {
    text_frame(format!("{DATA_PREFIX}{}{DATA_SUFFIX}", base64(bytes)).as_bytes())
        .expect("one output frame fits one WebSocket message")
}

/// The whole tuple of one grant: its channel, its PTY open, its agent and its PTY.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Grant {
    pub(crate) channel: ChannelKey,
    pub(crate) daemon_epoch: u64,
    pub(crate) agent: AgentId,
    pub(crate) pty: PtyId,
}

/// A browser terminal effect that passed its channel's session-lock check.
pub struct BrowserAppliedTerminalEffect(TerminalAction);

impl fmt::Debug for BrowserAppliedTerminalEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BrowserAppliedTerminalEffect(<redacted>)")
    }
}

impl BrowserAppliedTerminalEffect {
    pub(crate) fn into_action(self) -> TerminalAction {
        self.0
    }
}

pub(crate) enum TerminalAction {
    Attach {
        channel: ChannelKey,
        agent: AgentId,
        cols: u16,
        rows: u16,
    },
    Granted {
        grant: Grant,
        op: GrantedOp,
    },
}

pub(crate) enum GrantedOp {
    Write {
        bytes: Vec<u8>,
        payload: PayloadDigest,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Detach,
}

/// What an applied browser attach hands back to its channel.
pub struct BrowserAttachment {
    pub(crate) grant: Grant,
    revoked: Arc<AtomicBool>,
    output: OutputSubscription,
}

impl fmt::Debug for BrowserAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserAttachment")
            .field("pty", &self.grant.pty)
            .finish_non_exhaustive()
    }
}

/// The daemon loop's record of one live browser grant.
#[derive(Clone)]
pub(crate) struct LiveGrant {
    grant: Grant,
    revoked: Arc<AtomicBool>,
}

impl LiveGrant {
    pub(crate) fn opened(channel: ChannelKey, agent: AgentId, pty: PtyId) -> Self {
        let mut epoch = [0_u8; 8];
        getrandom::fill(&mut epoch).expect("the operating system's entropy source");
        Self {
            grant: Grant {
                channel,
                daemon_epoch: u64::from_le_bytes(epoch),
                agent,
                pty,
            },
            revoked: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn attachment(&self, output: OutputSubscription) -> BrowserAttachment {
        BrowserAttachment {
            grant: self.grant,
            revoked: Arc::clone(&self.revoked),
            output,
        }
    }

    pub(crate) fn admits(&self, presented: &Grant) -> bool {
        !self.revoked()
            && self.grant.channel.session_digest == presented.channel.session_digest
            && self.grant.channel.browser_epoch == presented.channel.browser_epoch
            && self.grant.daemon_epoch == presented.daemon_epoch
            && self.grant.agent == presented.agent
            && self.grant.pty == presented.pty
    }

    pub(crate) const fn agent(&self) -> AgentId {
        self.grant.agent
    }

    pub(crate) const fn pty(&self) -> PtyId {
        self.grant.pty
    }

    pub(crate) fn revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    /// Whether this call is the one that revoked the grant.
    #[must_use]
    pub(crate) fn revoke(&self) -> bool {
        !self.revoked.swap(true, Ordering::AcqRel)
    }
}

/// Which one field of the live grant [`TerminalSkew`] perturbs.
#[cfg(feature = "testing")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkewField {
    /// The session digest.
    Session,
    /// The browser epoch.
    BrowserEpoch,
    /// The daemon epoch.
    DaemonEpoch,
    /// The agent.
    Agent,
    /// The PTY.
    Pty,
}

/// Makes the loop's next grant check see one field of the live grant changed, so a test can show
/// that each comparison refuses on its own.
#[cfg(feature = "testing")]
#[derive(Clone, Debug, Default)]
pub struct TerminalSkew(Arc<std::sync::Mutex<Option<SkewField>>>);

#[cfg(feature = "testing")]
impl TerminalSkew {
    /// Perturb `field` in the next grant check, once.
    pub fn arm(&self, field: SkewField) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(field);
    }

    pub(crate) fn admits(&self, live: &LiveGrant, presented: &Grant) -> bool {
        let armed = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(field) = armed else {
            return live.admits(presented);
        };
        let mut skewed = live.clone();
        let grant = &mut skewed.grant;
        match field {
            SkewField::Session => {
                let mut digest = grant.channel.session_digest.bytes();
                digest[0] ^= 1;
                grant.channel.session_digest = dudes_proto::record::Hex32::new(digest);
            }
            SkewField::BrowserEpoch => grant.channel.browser_epoch ^= 1,
            SkewField::DaemonEpoch => grant.daemon_epoch ^= 1,
            SkewField::Agent => {
                let mut bytes = grant.agent.to_bytes();
                bytes[15] ^= 1;
                grant.agent = AgentId::from_bytes(bytes);
            }
            SkewField::Pty => grant.pty = PtyId(grant.pty.0 ^ 1),
        }
        skewed.admits(presented)
    }
}

#[derive(Debug)]
enum Ending {
    /// A malformed, unlisted or unadmitted client message.
    Policy,
    Closed(&'static str),
    PeerLeft,
    Failed,
}

impl From<io::Error> for Ending {
    fn from(_: io::Error) -> Self {
        Self::Failed
    }
}

impl Ending {
    /// The reason journaled on the system close of the grant this ending revokes.
    const fn reason(&self) -> &'static str {
        match self {
            Self::Closed(reason) => reason,
            Self::Policy | Self::PeerLeft => "terminal channel closed",
            Self::Failed => "terminal channel failed",
        }
    }
}

type Flow = Result<(), Ending>;

enum State {
    Idle,
    Attached(Attached),
}

struct Attached {
    grant: Grant,
    revoked: Arc<AtomicBool>,
    output: OutputSubscription,
}

pub(crate) fn serve(
    stream: &mut TcpStream,
    session: TerminalSession,
    buffered: &[u8],
    phase: &AtomicU8,
    handle: &Handle,
) {
    let mut channel = Channel {
        session,
        state: State::Idle,
        handle,
    };
    let Err(ending) = channel.drive(stream, buffered, phase);
    let frame = match ending {
        Ending::Policy => policy_close_frame(),
        Ending::Closed(_) => [message_frame(&ServerMessage::Closed), close_frame()].concat(),
        Ending::PeerLeft | Ending::Failed => close_frame(),
    };
    let _ = write_bytes(stream, &frame, phase);
    channel.release(ending.reason());
}

struct Channel<'a> {
    session: TerminalSession,
    state: State,
    handle: &'a Handle,
}

impl Channel<'_> {
    fn drive(
        &mut self,
        stream: &mut TcpStream,
        buffered: &[u8],
        phase: &AtomicU8,
    ) -> Result<Infallible, Ending> {
        stream.set_read_timeout(Some(ACCEPT_POLL))?;
        let mut decoder = Decoder::default();
        self.receive(decoder.push(buffered), stream, phase)?;
        let mut checked = Instant::now();
        let mut input = [0_u8; 8192];
        loop {
            if stop_requested(phase) {
                return Err(Ending::Closed("Floor listener stopped"));
            }
            if checked.elapsed() >= RECHECK_AFTER {
                if !self.live() {
                    return Err(Ending::Closed("Floor session ended"));
                }
                checked = Instant::now();
            }
            match stream.read(&mut input) {
                Ok(0) => return Err(Ending::PeerLeft),
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

    fn live(&self) -> bool {
        match self.state {
            State::Idle => self.session.is_valid(),
            State::Attached(_) => self.session.grant_live(),
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
                Message::Close => return Err(Ending::PeerLeft),
            }
        }
        Ok(())
    }

    fn client(&mut self, client: ClientMessage, stream: &mut TcpStream, phase: &AtomicU8) -> Flow {
        match client {
            ClientMessage::Attach { agent, cols, rows } => {
                self.attach(agent, cols, rows, stream, phase)
            }
            ClientMessage::Write { handle, data } => {
                let (bytes, payload) = decode_terminal_input(&data)
                    .filter(|(bytes, _)| bytes.len() <= MAX_INPUT_BYTES)
                    .ok_or(Ending::Policy)?;
                match self.granted(
                    &handle,
                    IdleClock::Advance,
                    GrantedOp::Write { bytes, payload },
                )? {
                    Answered::PtyWritten(PtyWriteOutcome::Complete { .. }) => Ok(()),
                    _ => Err(Ending::Closed("terminal write incomplete")),
                }
            }
            ClientMessage::Resize { handle, cols, rows } => {
                if !terminal_size_in_bounds(cols, rows) {
                    return Err(Ending::Policy);
                }
                self.granted(
                    &handle,
                    IdleClock::Advance,
                    GrantedOp::Resize { cols, rows },
                )
                .map(drop)
            }
            ClientMessage::Detach { handle } => {
                match self.granted(&handle, IdleClock::Hold, GrantedOp::Detach)? {
                    Answered::PtyClosed(
                        PtyCloseOutcome::Closed | PtyCloseOutcome::AlreadyClosed,
                    ) => {
                        self.state = State::Idle;
                        self.session.release_grant();
                        write_bytes(stream, &message_frame(&ServerMessage::Detached), phase)?;
                        Ok(())
                    }
                    _ => Err(Ending::Closed("terminal detach unconfirmed")),
                }
            }
        }
    }

    fn attach(
        &mut self,
        agent: AgentId,
        cols: u16,
        rows: u16,
        stream: &mut TcpStream,
        phase: &AtomicU8,
    ) -> Flow {
        if !matches!(self.state, State::Idle) || !terminal_size_in_bounds(cols, rows) {
            return Err(Ending::Policy);
        }
        if !self.session.touch() {
            return Err(Ending::Closed("Floor session ended"));
        }
        let attach = TerminalAction::Attach {
            channel: self.session.key(),
            agent,
            cols,
            rows,
        };
        let Ok(Answered::BrowserAttached(attachment)) = self.apply(attach) else {
            return Err(Ending::Closed("terminal attach refused"));
        };
        let handle = self.session.mint_grant(Arc::clone(&attachment.revoked));
        self.state = State::Attached(Attached {
            grant: attachment.grant,
            revoked: attachment.revoked,
            output: attachment.output,
        });
        let handle = handle.ok_or(Ending::Closed("Floor session ended"))?;
        write_bytes(
            stream,
            &message_frame(&ServerMessage::Attached { handle: &handle }),
            phase,
        )?;
        Ok(())
    }

    /// Apply `op` under this channel's grant, if `handle` names it under the session lock.
    fn granted(&self, handle: &str, idle: IdleClock, op: GrantedOp) -> Result<Answered, Ending> {
        let State::Attached(attached) = &self.state else {
            return Err(Ending::Policy);
        };
        if crate::floor_actions::decode_attempt(handle).is_err()
            || !self.session.admit(handle, idle)
        {
            return Err(Ending::Policy);
        }
        self.apply(TerminalAction::Granted {
            grant: attached.grant,
            op,
        })
        .map_err(|_| Ending::Closed("terminal effect refused"))
    }

    fn apply(&self, action: TerminalAction) -> Result<Answered, dudes_proto::rpc::RpcError> {
        self.handle
            .ask(Ask::BrowserTerminal(BrowserAppliedTerminalEffect(action)))
    }

    fn pump(&self, stream: &mut TcpStream, phase: &AtomicU8) -> Flow {
        let State::Attached(attached) = &self.state else {
            return Ok(());
        };
        loop {
            match attached.output.next(Duration::ZERO) {
                Output::Idle => return Ok(()),
                Output::Data(bytes) => {
                    if !self.session.grant_live() {
                        return Err(Ending::Closed("terminal grant revoked"));
                    }
                    write_bytes(stream, &data_frame(&bytes), phase)?;
                }
                Output::Ended => return Err(Ending::Closed("terminal output ended")),
                Output::Overflowed => return Err(Ending::Closed("terminal output overflowed")),
            }
        }
    }

    /// Revoke the grant, then have the loop close its PTY under the system actor.
    fn release(self, reason: &'static str) {
        if let State::Attached(attached) = self.state {
            attached.revoked.store(true, Ordering::Release);
            self.handle
                .close_attachment(attached.grant.agent, attached.grant.pty, reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_grant_requires_its_full_identity_tuple() {
        let channel = ChannelKey {
            session_digest: dudes_proto::record::Hex32::new([1; 32]),
            browser_epoch: 1,
        };
        let live = LiveGrant::opened(channel, AgentId::from_parts(1, 1), PtyId(1));
        assert!(live.admits(&live.grant));
        let mut changed = [live.grant; 5];
        changed[0].channel.session_digest = dudes_proto::record::Hex32::new([2; 32]);
        changed[1].channel.browser_epoch += 1;
        changed[2].daemon_epoch ^= 1;
        changed[3].agent = AgentId::from_parts(1, 2);
        changed[4].pty = PtyId(2);
        for presented in changed {
            assert!(!live.admits(&presented));
        }
        assert!(live.revoke());
        assert!(!live.admits(&live.grant));
    }

    #[test]
    fn only_the_four_listed_messages_decode() {
        let agent = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let decode = |text: &str| crate::floor_stream::decode_object::<ClientMessage>(text);
        for text in [
            format!(r#"{{"type":"terminal.attach","agent":"{agent}","cols":80,"rows":24}}"#),
            r#"{"type":"terminal.write","handle":"h","data":"AA=="}"#.to_owned(),
            r#"{"type":"terminal.resize","handle":"h","cols":1,"rows":1}"#.to_owned(),
            r#"{"type":"terminal.detach","handle":"h"}"#.to_owned(),
        ] {
            assert!(decode(&text).is_ok(), "{text}");
        }
        for text in [
            r#"{"type":"terminal.hire","agent":"x"}"#.to_owned(),
            r#"{"type":"terminal.detach","handle":"h","pty":3}"#.to_owned(),
            r#"["terminal.detach","h"]"#.to_owned(),
            format!(r#"{{"type":"terminal.attach","agent":"{agent}","cols":80}}"#),
            format!(r#"{{"type":"terminal.attach","agent":"{agent}","cols":-1,"rows":24}}"#),
            format!("{}{}", "[".repeat(17), "]".repeat(17)),
        ] {
            assert!(decode(&text).is_err(), "{text}");
        }
    }

    #[test]
    fn server_messages_carry_the_handle_only_on_attached() {
        let text = |frame: Vec<u8>| String::from_utf8(frame[2..].to_vec()).unwrap();
        assert_eq!(
            [
                text(message_frame(&ServerMessage::Attached { handle: "h" })),
                text(data_frame(&[0])),
                text(message_frame(&ServerMessage::Detached)),
                text(message_frame(&ServerMessage::Closed)),
            ],
            [
                r#"{"type":"terminal.attached","handle":"h"}"#,
                r#"{"type":"terminal.data","data":"AA=="}"#,
                r#"{"type":"terminal.detached"}"#,
                r#"{"type":"terminal.closed"}"#,
            ]
        );
    }

    #[test]
    fn a_full_terminal_queue_fits_eight_mebibytes_on_the_wire() {
        let frame = data_frame(&vec![7_u8; MAX_FRAME_BYTES]);
        assert_eq!(frame.len(), FULL_DATA_FRAME_BYTES);
        let frames = TERMINAL_QUEUE_BYTES / MAX_FRAME_BYTES;
        assert!(frames * frame.len() <= WIRE_BOUND_BYTES);
        assert!((frames + 1) * frame.len() > WIRE_BOUND_BYTES);
    }
}
