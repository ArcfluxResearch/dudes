//! The synchronous client: connect, send a frame, read the answer.
//!
//! Synchronous because the hook shim is the client with the tightest budget — a static binary that
//! sends one request and exits — and because invariant 9 forbids this crate a runtime anyway. There
//! is no handshake and no header frame: an agent-side client opens one connection per call,
//! so there is no connection-level state worth negotiating.

use std::io::{BufRead, Write};

use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::{
    CurrentMethod, JSONRPC, Method, Notification, Request, RequestId, Response,
};
use serde_json::Value;

use crate::codec::{FrameError, read_frame, write_frame};

/// A frame the daemon sent: an answer to a call, or a notification nobody asked for by id.
///
/// The two are told apart structurally rather than by a flag. A [`Response`] must carry an `id` and
/// a [`Notification`] must carry a `method`, and neither envelope has the other's required field,
/// so a frame decodes as exactly one of them.
#[derive(Clone, Debug, PartialEq)]
pub enum Inbound {
    /// An answer to a call, carrying the id of the request it answers.
    Response(Response),
    /// A subscription's message: `journal.appended` and the rest.
    Notification(Notification),
}

/// A call that did not produce a response.
///
/// A JSON-RPC *error* is not one of these: that is a [`Response`] carrying an `error`, which is an
/// answer. These are the ways the conversation itself failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The frame could not be written or read.
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// The stream itself failed.
    #[error("client i/o: {0}")]
    Io(#[from] std::io::Error),
    /// The connection closed before the answer arrived.
    #[error("the daemon closed the connection without answering")]
    Closed,
    /// The answer was not a JSON-RPC response.
    #[error("the daemon's answer was not a response: {0}")]
    Malformed(#[source] serde_json::Error),
}

/// A connection to the daemon.
///
/// Holds the connection open so several calls can share it. That is what a
/// terminal-attached operator wants; an agent-side caller that makes exactly one call may simply
/// drop it after that call.
#[derive(Debug)]
pub struct Client<R, W> {
    reader: R,
    writer: W,
    next_id: i64,
}

/// The platform's local control-plane client.
///
/// Both platforms expose the same buffered-reader/write-half shape. The concrete stream stays in
/// this crate so consumers never name a Unix socket or a Windows named pipe.
#[cfg(unix)]
pub type LocalClient =
    Client<std::io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream>;

/// The platform's local control-plane client.
#[cfg(windows)]
pub type LocalClient = Client<
    std::io::BufReader<
        interprocess::os::windows::named_pipe::RecvPipeStream<
            interprocess::os::windows::named_pipe::pipe_mode::Bytes,
        >,
    >,
    interprocess::os::windows::named_pipe::SendPipeStream<
        interprocess::os::windows::named_pipe::pipe_mode::Bytes,
    >,
>;

#[cfg(windows)]
type WindowsBytePipe = interprocess::os::windows::named_pipe::DuplexPipeStream<
    interprocess::os::windows::named_pipe::pipe_mode::Bytes,
>;

/// Connect, inspect the still-whole pipe, then expose its read and write halves.
///
/// Keeping inspection before `split` makes refusal structural: an error drops the only stream and
/// no writer capable of presenting a credential can escape this function.
#[cfg(windows)]
fn connect_windows_with_inspector(
    path: impl AsRef<std::path::Path>,
    inspect: impl FnOnce(&WindowsBytePipe) -> std::io::Result<()>,
) -> std::io::Result<LocalClient> {
    let stream = WindowsBytePipe::connect_by_path(path.as_ref())?;
    inspect(&stream)?;
    let (reader, writer) = stream.split();
    Ok(Client::new(std::io::BufReader::new(reader), writer))
}

#[cfg(windows)]
fn require_current_user_pipe(stream: &WindowsBytePipe, expected_sid: &str) -> std::io::Result<()> {
    use std::os::windows::io::AsHandle as _;

    let owner = dudes_win::read_owner(stream.as_handle())?;
    let dacl = dudes_win::read_dacl(stream.as_handle())?;
    let expected_dacl = dudes_win::Dacl::Present {
        allowed: vec![expected_sid.to_owned()],
        denied: Vec::new(),
        unread: 0,
    };
    if owner == expected_sid && dacl == expected_dacl {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the connected named pipe is not owned solely by the current Windows user",
        ))
    }
}

#[cfg(unix)]
impl Client<std::io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream> {
    /// Connect to the control socket at `path`.
    ///
    /// # Errors
    ///
    /// Whatever connecting returns — including "no such file", which is what a daemon that is not
    /// running looks like.
    pub fn connect(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(path)?;
        let reader = std::io::BufReader::new(stream.try_clone()?);
        Ok(Self::new(reader, stream))
    }
}

#[cfg(windows)]
impl
    Client<
        std::io::BufReader<
            interprocess::os::windows::named_pipe::RecvPipeStream<
                interprocess::os::windows::named_pipe::pipe_mode::Bytes,
            >,
        >,
        interprocess::os::windows::named_pipe::SendPipeStream<
            interprocess::os::windows::named_pipe::pipe_mode::Bytes,
        >,
    >
{
    /// Connect to the local named pipe at `path`.
    ///
    /// # Errors
    ///
    /// Whatever reading the current Windows user, connecting, or reading the connected pipe's
    /// owner and DACL returns; [`std::io::ErrorKind::PermissionDenied`] when the pipe is not owned
    /// solely by that user.
    pub fn connect(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        let expected_sid = dudes_win::current_user_sid()?;
        connect_windows_with_inspector(path, |stream| {
            require_current_user_pipe(stream, &expected_sid)
        })
    }
}

impl<R: BufRead, W: Write> Client<R, W> {
    /// A client over an already-connected pair of halves.
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            next_id: 1,
        }
    }

    /// Call `method` with `params` and read the answer.
    ///
    /// # Errors
    ///
    /// [`ClientError`] when the conversation failed. A method that the daemon *refused* returns
    /// `Ok` with an `error` in the response — a refusal is an answer.
    pub fn call(&mut self, method: Method, params: Option<Value>) -> Result<Response, ClientError> {
        self.send(method, params)?;
        self.read_response()
    }

    /// Call `method` with `params`, presenting `cred` on the envelope, and read the answer.
    ///
    /// The credential is passed per call rather than held on the client, and that is not an
    /// oversight: [`OperatorCredential`] has no `Clone`, so a client that stored one could not
    /// stamp it onto each request without either duplicating the secret behind the caller's back
    /// or handing out a reference to it. A caller that sends many operator calls builds a
    /// credential from the text it read once, per call, which is one visible line at each site
    /// that presents a secret.
    ///
    /// # Errors
    ///
    /// [`ClientError`] when the conversation failed. A refusal is an answer, not an error.
    pub fn call_as_operator(
        &mut self,
        method: Method,
        params: Option<Value>,
        cred: OperatorCredential,
    ) -> Result<Response, ClientError> {
        self.call_current_as_operator(method.into(), params, cred)
    }

    /// Call a current contract method while presenting an operator credential.
    ///
    /// # Errors
    ///
    /// [`ClientError`] when the conversation failed. A refusal is an answer.
    pub fn call_current_as_operator(
        &mut self,
        method: CurrentMethod,
        params: Option<Value>,
        cred: OperatorCredential,
    ) -> Result<Response, ClientError> {
        self.call_current_presenting(method, params, Some(cred))
    }

    /// Call a current method with the caller's available operator presentation.
    ///
    /// # Errors
    ///
    /// [`ClientError`] when the conversation failed. Missing credentials remain a server refusal.
    pub fn call_current_presenting(
        &mut self,
        method: CurrentMethod,
        params: Option<Value>,
        cred: Option<OperatorCredential>,
    ) -> Result<Response, ClientError> {
        self.send_current_presenting(method, params, cred)?;
        self.read_response()
    }

    /// Send a request and return its id **without** reading anything.
    ///
    /// What a subscribing caller needs: `journal.tail`'s backfill reaches the peer before the
    /// response to the call that asked for it, so a caller that read one frame here would be
    /// handed a notification and would have to put it back. Send, then read frames with
    /// [`Client::recv`].
    ///
    /// # Errors
    ///
    /// [`ClientError`] when the frame could not be written.
    pub fn send(
        &mut self,
        method: Method,
        params: Option<Value>,
    ) -> Result<RequestId, ClientError> {
        self.send_presenting(method, params, None)
    }

    /// As [`Client::send`], presenting `cred` on the envelope when there is one.
    ///
    /// # Errors
    ///
    /// [`ClientError`] when the frame could not be written.
    pub fn send_presenting(
        &mut self,
        method: Method,
        params: Option<Value>,
        cred: Option<OperatorCredential>,
    ) -> Result<RequestId, ClientError> {
        self.send_current_presenting(method.into(), params, cred)
    }

    fn send_current_presenting(
        &mut self,
        method: CurrentMethod,
        params: Option<Value>,
        cred: Option<OperatorCredential>,
    ) -> Result<RequestId, ClientError> {
        let id = RequestId::Number(self.next_id);
        self.next_id += 1;
        let request = Request {
            jsonrpc: JSONRPC.to_owned(),
            id: id.clone(),
            method: method.name().to_owned(),
            params,
            v: None,
            cred,
        };
        if method == CurrentMethod::Legacy(Method::GoalReplace) {
            let bytes = serde_json::to_vec(&request)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
            if bytes.len() > dudes_proto::rpc::goal::MAX_GOAL_REPLACE_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "goal.replace frame exceeds 128 KiB",
                )
                .into());
            }
        }
        write_frame(&mut self.writer, &request)?;
        Ok(id)
    }

    /// Send `line` verbatim as one frame and read the answer.
    ///
    /// The escape hatch for text the typed API cannot build — a malformed frame, an unknown method
    /// name, a hand-written envelope. It exists so the transport's behaviour on bad input is
    /// testable at all; nothing in the daemon should reach for it.
    ///
    /// # Errors
    ///
    /// As [`Client::call`].
    pub fn send_raw(&mut self, line: &str) -> Result<Response, ClientError> {
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        self.read_response()
    }

    /// Read the next frame, whatever it is.
    ///
    /// [`Client::call`] is the wrong tool for a subscription: it reads exactly one frame and
    /// insists it be a [`Response`], so the first `journal.appended` a subscriber receives would be
    /// reported as [`ClientError::Malformed`]. A subscribing caller reads frames instead and sorts
    /// them here.
    ///
    /// **This does no id matching**, so a caller that both calls and subscribes on one connection
    /// must do its own — see [`Client::call`]'s own note about the same gap.
    ///
    /// # Errors
    ///
    /// [`ClientError`] when the conversation failed.
    pub fn recv(&mut self) -> Result<Inbound, ClientError> {
        let frame = read_frame(&mut self.reader)?.ok_or(ClientError::Closed)?;
        if let Ok(response) = serde_json::from_str::<Response>(&frame) {
            return Ok(Inbound::Response(response));
        }
        serde_json::from_str::<Notification>(&frame)
            .map(Inbound::Notification)
            .map_err(ClientError::Malformed)
    }

    fn read_response(&mut self) -> Result<Response, ClientError> {
        let frame = read_frame(&mut self.reader)?.ok_or(ClientError::Closed)?;
        serde_json::from_str(&frame).map_err(ClientError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn goal_replacement_checks_its_complete_outgoing_frame_before_writing() {
        let mut client = super::Client::new(std::io::Cursor::new(Vec::<u8>::new()), Vec::new());
        client
            .send(
                dudes_proto::rpc::Method::GoalReplace,
                Some(serde_json::json!({"control":true})),
            )
            .unwrap();
        let before = client.writer.clone();
        assert!(
            client
                .send(
                    dudes_proto::rpc::Method::GoalReplace,
                    Some(serde_json::json!({"padding":" ".repeat(131_072)}))
                )
                .is_err()
        );
        assert_eq!(client.writer, before);
        assert!(before.ends_with(b"\n"));
    }

    use std::io::Cursor;

    use serde_json::json;

    use super::*;

    #[test]
    fn current_and_legacy_operator_calls_share_the_typed_frame_path() {
        let replies = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"checkout\":true}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ping\":true}}\n",
        );
        let mut client = Client::new(Cursor::new(replies.as_bytes()), Vec::new());
        let credential = || OperatorCredential::new("C".repeat(43)).unwrap();
        let current = client
            .call_current_as_operator(
                CurrentMethod::TicketCheckout,
                Some(json!({"card": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "expected_seq": 7})),
                credential(),
            )
            .unwrap();
        let legacy = client
            .call_as_operator(Method::SystemPing, None, credential())
            .unwrap();
        assert_eq!(current.result.unwrap(), json!({"checkout": true}));
        assert_eq!(legacy.result.unwrap(), json!({"ping": true}));

        let frames: Vec<Value> = std::str::from_utf8(&client.writer)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            frames,
            vec![
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "ticket.checkout",
                    "params": {"card": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "expected_seq": 7},
                    "cred": "C".repeat(43),
                }),
                json!({
                    "jsonrpc": "2.0", "id": 2, "method": "system.ping",
                    "cred": "C".repeat(43),
                }),
            ]
        );
    }

    #[test]
    fn current_operator_goal_replacement_refuses_oversized_frames_before_writing() {
        let mut client = Client::new(Cursor::new(Vec::<u8>::new()), Vec::new());
        let error = client
            .call_current_as_operator(
                CurrentMethod::Legacy(Method::GoalReplace),
                Some(json!({"padding": " ".repeat(131_072)})),
                OperatorCredential::new("C".repeat(43)).unwrap(),
            )
            .unwrap_err();
        assert!(
            matches!(error, ClientError::Io(ref err) if err.kind() == std::io::ErrorKind::InvalidInput)
        );
        assert!(client.writer.is_empty());
        client.send(Method::SystemPing, None).unwrap();
        let written: Value = serde_json::from_slice(&client.writer).unwrap();
        assert_eq!(written["method"], "system.ping");
        assert_eq!(written["id"], 2);
    }

    /// The two envelopes are told apart, and neither is read as the other. The pair is the point:
    /// a classifier that answered `Notification` to everything would satisfy either arm alone.
    #[test]
    fn a_response_and_a_notification_are_told_apart() {
        let frames = format!(
            "{}\n{}\n",
            json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": true } }),
            json!({ "jsonrpc": "2.0", "method": "journal.appended", "params": { "lsn": 7 } }),
        );
        let mut client = Client::new(Cursor::new(frames.into_bytes()), Vec::new());

        let Inbound::Response(response) = client.recv().unwrap() else {
            panic!("the first frame is a response")
        };
        assert_eq!(response.id, RequestId::Number(1));

        let Inbound::Notification(notification) = client.recv().unwrap() else {
            panic!("the second frame is a notification")
        };
        assert_eq!(notification.method, "journal.appended");
    }

    /// A frame that is neither is reported rather than guessed at.
    #[test]
    fn a_frame_that_is_neither_envelope_is_malformed() {
        let mut client = Client::new(Cursor::new(b"{\"nope\":1}\n".to_vec()), Vec::new());
        assert!(matches!(client.recv(), Err(ClientError::Malformed(_))));
    }

    /// A platform identity read that refuses closes the connected pipe before a writer can escape.
    #[cfg(windows)]
    #[test]
    fn a_failed_pipe_identity_read_returns_connect_refusal() {
        use std::io::Read as _;
        use std::sync::mpsc;
        use std::time::Duration;

        use interprocess::os::windows::named_pipe::{PipeListenerOptions, PipeMode, pipe_mode};

        let root = tempfile::tempdir().expect("temporary root");
        let endpoint = crate::local_endpoint(root.path(), "read-failure.sock");
        let listener = PipeListenerOptions::new()
            .path(endpoint.as_path())
            .mode(PipeMode::Bytes)
            .accept_remote(false)
            .create_duplex::<pipe_mode::Bytes>()
            .expect("bind test pipe");
        let (accepted, acceptance) = mpsc::channel();
        let (read, observed) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().expect("accept client");
            accepted
                .send(())
                .expect("report that the server accepted the client");
            let mut byte = [0u8; 1];
            read.send(stream.read(&mut byte).expect("read after refusal"))
                .expect("report read");
        });

        let error = connect_windows_with_inspector(&endpoint, |_| {
            // `interprocess` clears a client that disconnects before `accept` observes it and then
            // waits for another. Keep this client live until the server owns the accepted stream;
            // only then inject the identity-read refusal whose drop must produce EOF with no bytes.
            acceptance
                .recv_timeout(Duration::from_secs(5))
                .expect("server accepted the client before identity refusal");
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected pipe identity read refusal",
            ))
        })
        .expect_err("a failed identity read refuses the connection");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(5))
                .expect("server observed client close"),
            0,
            "the refused client writes no request bytes"
        );
        server.join().expect("join server");
    }
}
