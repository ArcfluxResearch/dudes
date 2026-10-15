//! What `serve` owes after `stop`, and what a live connection costs while it holds one.
//!
//! Every test here asserts on a **channel with a deadline**, never on a join. That is not a style
//! choice: the defect these tests exist for is a `serve` that never returns, so a test that joined
//! the shutdown thread would *hang* rather than fail — a wedged run is not a red transcript, and CI
//! would report a timeout for the whole binary instead of the one property that broke.
//!
//! The four properties are one mechanism seen from four sides — a registry of live connections that
//! `stop` can reach — so they share a file rather than being scattered by the name of the symbol
//! each happens to touch.

//! Unix-only: these drive the loopback harness, which binds a real Unix socket and is exported only
//! on Unix. Gated at the file rather than per test, because the `use` above the first test is what
//! a Windows build fails on.
#![cfg(unix)]

use std::io::{self, BufRead, BufReader, Read, Write as _};
use std::net::Shutdown;
use std::num::NonZeroUsize;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use dudes_proto::record::{Entry, Record};
use dudes_proto::rpc::Method;
use dudes_rpc::testing::{AdmitAll, FakeResolver, TestServer};
use dudes_rpc::{
    Client, ClientError, Connection, Dispatcher, FrameError, Halt, HangUp, Listener,
    LocalTransport, PeerIdentity, Requirement, StopFlag, Subscribers,
};
use dudes_types::{AgentId, Lsn, Timestamp};
use serde_json::json;
use tempfile::TempDir;

/// How long `serve` is given to return after `stop` before the test concludes it is not going to.
///
/// Orders of magnitude above what the fixed path costs — a halt is one `shutdown(2)` per live
/// connection and the threads then return from a read that has already failed — so reaching it is
/// not a scheduling accident. It is paid only on the failing path.
const BOUND: Duration = Duration::from_secs(5);

/// How long a call is given before the test concludes the daemon is not answering it.
const ANSWER_DEADLINE: Duration = Duration::from_secs(5);

/// How long a blocked appender is given to make progress before the test concludes it is wedged
/// rather than slow. The shape [`dudes_rpc::Subscribers`]'s own tests already use.
const BLOCKED_FOR: Duration = Duration::from_millis(250);

/// The size of one fan-out payload. Large enough that a handful of them exceed any platform's
/// socket buffer, so a subscriber that stops reading wedges the fan-out in a bounded number of
/// appends rather than in an unbounded one.
const PAYLOAD_BYTES: usize = 64 * 1024;

/// How many appends the fan-out thread offers. `APPENDS * PAYLOAD_BYTES` is 8 MiB, far past any
/// socket buffer; an arm that gets through all of them without blocking has not wedged, and says so
/// as a failed assertion rather than passing quietly.
const APPENDS: u64 = 128;

struct NextReadGate {
    reached: mpsc::Sender<()>,
    released: mpsc::Receiver<()>,
}

struct GatedReader {
    inner: BufReader<UnixStream>,
    gate: Option<NextReadGate>,
    first_frame_read: bool,
}

impl Read for GatedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let count = available.len().min(buf.len());
        buf[..count].copy_from_slice(&available[..count]);
        self.consume(count);
        Ok(count)
    }
}

impl BufRead for GatedReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.first_frame_read
            && let Some(gate) = self.gate.take()
        {
            gate.reached.send(()).expect("the reader gate is observed");
            let _ = gate.released.recv();
        }
        self.inner.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.first_frame_read |= self.inner.buffer()[..amount].contains(&b'\n');
        self.inner.consume(amount);
    }
}

struct GatedConnection {
    stream: UnixStream,
    gate: Option<NextReadGate>,
}

impl Connection for GatedConnection {
    type Reader = GatedReader;
    type Writer = <UnixStream as Connection>::Writer;

    fn split(self) -> io::Result<(Self::Reader, Self::Writer)> {
        let (reader, writer, _) = self.split_with_hang_up()?;
        Ok((reader, writer))
    }

    fn halt_handle(&self) -> io::Result<Box<dyn Halt>> {
        self.stream.halt_handle()
    }

    fn split_with_hang_up(self) -> io::Result<(Self::Reader, Self::Writer, HangUp)> {
        let (inner, writer, hang_up) = self.stream.split_with_hang_up()?;
        Ok((
            GatedReader {
                inner,
                gate: self.gate,
                first_frame_read: false,
            },
            writer,
            hang_up,
        ))
    }
}

struct GatedListener {
    inner: LocalTransport,
    gate: Mutex<Option<NextReadGate>>,
}

impl Listener for GatedListener {
    type Conn = GatedConnection;

    fn accept(&self) -> io::Result<(Self::Conn, PeerIdentity)> {
        let (stream, peer) = self.inner.accept()?;
        Ok((
            GatedConnection {
                stream,
                gate: self.gate.lock().unwrap().take(),
            },
            peer,
        ))
    }
}

struct GatedServer {
    socket: std::path::PathBuf,
    stop: Arc<StopFlag>,
    reached: mpsc::Receiver<()>,
    release: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl GatedServer {
    fn start(dir: &TempDir) -> Self {
        let inner = LocalTransport::bind_in(dir.path().join("gated"), "duded.sock").unwrap();
        let socket = inner.path().to_path_buf();
        let (reached, observed) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let listener = GatedListener {
            inner,
            gate: Mutex::new(Some(NextReadGate { reached, released })),
        };
        let stop = Arc::new(StopFlag::new());
        let serving = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let mut dispatcher = Dispatcher::new(AdmitAll);
            dispatcher.register(Method::SystemPing, Requirement::AnyRole, |_| {
                Ok(json!({ "pong": true }))
            });
            dudes_rpc::serve(
                &listener,
                &FakeResolver::new(dudes_proto::PeerProvenance::Unresolvable),
                &dispatcher,
                &serving,
                NonZeroUsize::MIN,
            )
            .expect("the gated server stops cleanly");
        });
        Self {
            socket,
            stop,
            reached: observed,
            release: Some(release),
            thread: Some(thread),
        }
    }
}

impl Drop for GatedServer {
    fn drop(&mut self) {
        self.release.take();
        self.stop.stop();
        let _ = UnixStream::connect(&self.socket);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the gated server exits");
        }
    }
}

#[test]
fn a_response_alone_does_not_order_connection_teardown() {
    let dir = tempfile::tempdir().unwrap();
    let server = GatedServer::start(&dir);
    let stream = UnixStream::connect(&server.socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut client = Client::new(
        BufReader::new(stream.try_clone().unwrap()),
        stream.try_clone().unwrap(),
    );
    let response = client.call(Method::SystemPing, None).unwrap();
    assert_eq!(response.result, Some(json!({ "pong": true })));
    server.reached.recv_timeout(BOUND).unwrap();
    assert_eq!(server.stop.live_connections(), 1);
    stream.shutdown(Shutdown::Write).unwrap();
    let result = client.recv();
    writeln!(std::io::stderr(), "held teardown response: {result:?}").unwrap();
    assert!(
        matches!(&result, Err(ClientError::Frame(FrameError::Io(error)))
            if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)),
        "{result:?}"
    );
    assert_eq!(server.stop.live_connections(), 1);
}

fn await_rpc_connections(count: &dyn Fn() -> usize, expected: usize) {
    let deadline = Instant::now() + BOUND;
    loop {
        let observed = count();
        if observed == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "expected {expected} RPC connections, observed {observed}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_released_connection_slot_orders_the_first_clean_close() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = GatedServer::start(&dir);
    let stream = UnixStream::connect(&server.socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut client = Client::new(
        BufReader::new(stream.try_clone().unwrap()),
        stream.try_clone().unwrap(),
    );
    let response = client.call(Method::SystemPing, None).unwrap();
    assert_eq!(response.result, Some(json!({ "pong": true })));
    server.reached.recv_timeout(BOUND).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();

    thread::scope(|scope| {
        let release = server.release.take();
        let stop = Arc::clone(&server.stop);
        let (observations, observed) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let waiting = scope.spawn(move || {
            let reads = std::cell::Cell::new(0);
            await_rpc_connections(
                &|| {
                    let count = stop.live_connections();
                    if reads.get() < 2 {
                        reads.set(reads.get() + 1);
                        observations.send(count).unwrap();
                    }
                    count
                },
                0,
            );
            finished.send(()).unwrap();
        });
        assert_eq!(observed.recv_timeout(BOUND), Ok(1));
        assert_eq!(observed.recv_timeout(BOUND), Ok(1));
        drop(release);
        done.recv_timeout(BOUND).unwrap();
        let result = client.recv();
        writeln!(
            std::io::stderr(),
            "released teardown first read: {result:?}"
        )
        .unwrap();
        assert!(matches!(result, Err(ClientError::Closed)), "{result:?}");
        waiting.join().unwrap();
    });
}

/// A server carrying two methods and the registry the daemon would append through.
///
/// The authorizer is [`AdmitAll`], which resolves every peer to `PeerRole::Unproven`, and both
/// methods are registered `AnyRole`. **That is deliberate and this file is the reason**: what varies
/// here is the lifecycle, so authorisation is held constant at the widest setting rather than being
/// a second thing each test could fail on. Which roles `journal.tail` really admits is
/// `tests/requirements.rs`'s subject, asserted against the registration `methods::register_all`
/// makes.
fn started(dir: &TempDir) -> (TestServer, Arc<Subscribers>) {
    started_with_cap(dir, TestServer::CAP)
}

/// As [`started`], with the connection cap under test.
fn started_with_cap(dir: &TempDir, cap: NonZeroUsize) -> (TestServer, Arc<Subscribers>) {
    let subscribers = Arc::new(Subscribers::new());
    let mut dispatcher = Dispatcher::new(AdmitAll);
    dispatcher.register(Method::SystemPing, Requirement::AnyRole, |_| {
        Ok(json!({ "pong": true }))
    });
    let registry = Arc::clone(&subscribers);
    dispatcher.register(Method::JournalTail, Requirement::AnyRole, move |call| {
        registry.subscribe(call.outbound, || Ok(Vec::new()))?;
        Ok(json!({}))
    });
    let server = TestServer::start_with_cap(dir.path().join("run"), dispatcher, cap);
    (server, subscribers)
}

/// A client on its own connection, whose reads have a deadline.
///
/// Not the harness's own `client()`: a read with no deadline turns "the daemon never answered" into
/// a hung binary, which is the failure mode this whole file is written against.
fn client(server: &TestServer) -> Client<BufReader<UnixStream>, UnixStream> {
    let stream = UnixStream::connect(server.socket()).expect("connect to the test socket");
    stream.set_read_timeout(Some(ANSWER_DEADLINE)).unwrap();
    let reader = BufReader::new(stream.try_clone().unwrap());
    Client::new(reader, stream)
}

/// Run `server.shutdown()` on its own thread and report whether it returned within [`BOUND`].
///
/// The server is moved in, so a shutdown that never returns leaks one parked thread instead of
/// blocking the test that measured it.
fn shutdown_within(server: TestServer) -> Result<(), RecvTimeoutError> {
    let (finished, returned) = mpsc::channel();
    thread::spawn(move || {
        let mut server = server;
        server.shutdown();
        // Ignored: if the assertion has already failed its receiver is gone, the test is red
        // either way, and a second panic would only bury the first.
        let _ = finished.send(());
    });
    returned.recv_timeout(BOUND)
}

/// An entry whose payload is [`PAYLOAD_BYTES`] long and names its own lsn, so a fan-out that
/// delivered the same entry repeatedly could still be told apart from one that delivered a
/// sequence.
fn fat_entry(lsn: u64) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_000),
        record: Record::Steer {
            agent: AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            text: format!("{lsn}:{}", "x".repeat(PAYLOAD_BYTES)),
            provenance: None,
        },
    }
}

/// Subscribe a connection to `journal.appended` and return it, still open, without reading a byte
/// of what it is sent.
///
/// The subscription is confirmed against the registry before returning: appending to a registry the
/// subscriber has not reached yet would wedge nothing, and the test would pass having measured
/// nothing.
fn subscribed(server: &TestServer, subscribers: &Subscribers) -> UnixStream {
    let stream = UnixStream::connect(server.socket()).expect("connect to the test socket");
    let mut client = Client::new(
        BufReader::new(stream.try_clone().unwrap()),
        stream.try_clone().unwrap(),
    );
    client
        .send(Method::JournalTail, Some(json!({})))
        .expect("the subscription request goes out");

    let deadline = Instant::now() + ANSWER_DEADLINE;
    while subscribers.is_empty() {
        assert!(
            Instant::now() < deadline,
            "the subscription never reached the registry"
        );
        thread::sleep(Duration::from_millis(5));
    }
    stream
}

/// Offer [`APPENDS`] fat entries to `subscribers` from another thread.
///
/// Returns a channel that carries one lsn per **completed** append and a channel that carries a
/// single `()` when the last one is done. Progress is reported after the call returns rather than
/// before it, so a silent channel means the fan-out is inside a blocking write and not merely
/// waiting to start.
fn fan_out(subscribers: &Arc<Subscribers>) -> (mpsc::Receiver<u64>, mpsc::Receiver<()>) {
    let (progress, appended) = mpsc::channel();
    let (finished, done) = mpsc::channel();
    let subscribers = Arc::clone(subscribers);
    thread::spawn(move || {
        for lsn in 1..=APPENDS {
            subscribers.appended(Lsn(lsn), &fat_entry(lsn));
            let _ = progress.send(lsn);
        }
        let _ = finished.send(());
    });
    (appended, done)
}

/// What a fresh connection got out of the daemon.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Probe {
    /// `system.ping` was answered.
    Answered,
    /// The connection ended without an answer. The refusal shape this transport uses: the contract's
    /// `ErrorCode` is an exhaustive public enum with no code meaning "too many connections", so
    /// answering with a frame would be a wire-format change — and writing one on the accept thread
    /// is the class of blocking write this whole file is about.
    Ended,
}

/// Whether `kind` is a connection that ended rather than a conversation that failed.
///
/// **A refused connection does not always reach its peer as a clean end of stream**, and assuming
/// it did makes tests of this flaky. `shutdown(2)` on a
/// socket that still has the peer's unread request sitting in the daemon's receive queue resets it
/// rather than closing it, so what the peer sees depends on how far it had got: an end of stream, a
/// `BrokenPipe` from its own write, or a `NotConnected` from its read. All three are the same event.
///
/// `TimedOut` and `WouldBlock` are deliberately **not** here: a read that hit the deadline is a
/// daemon that neither answered nor refused, which is a finding rather than a refusal.
fn is_ended(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::UnexpectedEof
    )
}

/// Open a connection, call `system.ping` on it, and classify what came back.
///
/// The stream is handed back still open, because the caller decides when the connection ends: a
/// probe that dropped its own connection would free the very slot it was measuring.
///
/// # Panics
///
/// When the daemon neither answered nor ended the connection within [`ANSWER_DEADLINE`]. That is
/// the case worth failing on — a cap implemented by simply not accepting would leave the peer
/// parked in the listen backlog, which is neither a refusal nor an answer.
fn ping(server: &TestServer) -> (UnixStream, Probe) {
    let stream = UnixStream::connect(server.socket()).expect("connect to the test socket");
    if stream.set_read_timeout(Some(ANSWER_DEADLINE)).is_err() {
        // The refusal beat the probe to its own deadline: `setsockopt` on a socket the daemon has
        // already halted answers `EINVAL`. Reading it as anything but an ended connection would be
        // a lie, and `a_connection_within_the_cap_is_answered` is the control that stops this arm
        // swallowing a real fault — a probe that could never arm itself fails there, where an
        // answer is required.
        return (stream, Probe::Ended);
    }
    let mut caller = Client::new(
        BufReader::new(stream.try_clone().unwrap()),
        stream.try_clone().unwrap(),
    );
    let probe = match caller.call(Method::SystemPing, None) {
        Ok(response) => {
            assert!(response.error.is_none(), "{:?}", response.error);
            Probe::Answered
        }
        Err(ClientError::Closed) => Probe::Ended,
        Err(ClientError::Frame(FrameError::Io(err)) | ClientError::Io(err))
            if is_ended(err.kind()) =>
        {
            Probe::Ended
        }
        Err(other) => panic!("the daemon neither answered nor ended the connection: {other}"),
    };
    (stream, probe)
}

/// A connection cap exists, and the one over it is refused rather than given a thread.
///
/// Two connections are answered first and that is the load-bearing control: a transport that
/// refused *everything* would satisfy the third assertion perfectly, and the two look identical
/// from the refusal alone — the same argument `loopback.rs`'s non-UTF-8 test makes about a silent
/// disconnection.
///
/// The refusal is asserted as an **ended connection** rather than as a timeout, which is what
/// separates "refused" from "accepted and never served". A cap implemented by simply not accepting
/// would leave the third peer parked in the listen backlog, and [`ping`] would panic on the
/// deadline instead of returning [`Probe::Ended`].
#[test]
fn a_connection_over_the_cap_is_refused() {
    let dir = TempDir::new().unwrap();
    let (server, _subscribers) = started_with_cap(&dir, NonZeroUsize::new(2).unwrap());

    let (first, probe) = ping(&server);
    assert_eq!(probe, Probe::Answered, "control: the first is answered");
    let (second, probe) = ping(&server);
    assert_eq!(probe, Probe::Answered, "control: the second is answered");

    let (_third, probe) = ping(&server);
    assert_eq!(
        probe,
        Probe::Ended,
        "the connection over the cap was served instead of being refused"
    );

    drop((first, second));
}

/// The other arm of the same case: with the cap one higher, the same third connection **is**
/// answered.
///
/// This is "removing the cap turns it red" written as a passing arm, so the control lives in the
/// file rather than in a transcript nobody re-runs. It fails if the cap is off by one, if it counts
/// something other than live connections, or if it refuses on a number it was not given.
#[test]
fn a_connection_within_the_cap_is_answered() {
    let dir = TempDir::new().unwrap();
    let (server, _subscribers) = started_with_cap(&dir, NonZeroUsize::new(3).unwrap());

    let (first, _) = ping(&server);
    let (second, _) = ping(&server);
    let (_third, probe) = ping(&server);
    assert_eq!(
        probe,
        Probe::Answered,
        "the third connection is within a cap of three"
    );

    drop((first, second));
}

/// A slot is freed when its connection ends, so the cap bounds what is live rather than what has
/// ever connected.
///
/// Without this, a cap that leaked a slot per connection would pass both arms above and then refuse
/// every caller for the rest of the daemon's life — the failure being indistinguishable, from
/// outside, from a daemon that had simply stopped answering.
///
/// The retry loop is not a sleep in disguise: the server frees the slot when its connection thread
/// notices the peer has gone, which happens at its own pace, so the test polls to a deadline and
/// fails on the deadline rather than on the first attempt.
#[test]
fn a_slot_is_freed_when_its_connection_ends() {
    let dir = TempDir::new().unwrap();
    let (server, _subscribers) = started_with_cap(&dir, NonZeroUsize::new(2).unwrap());

    let (first, _) = ping(&server);
    let (second, probe) = ping(&server);
    assert_eq!(probe, Probe::Answered, "control: the cap is not yet hit");
    let (_over, probe) = ping(&server);
    assert_eq!(probe, Probe::Ended, "control: the cap is hit");

    drop(second);

    let deadline = Instant::now() + ANSWER_DEADLINE;
    loop {
        let (_replacement, probe) = ping(&server);
        if probe == Probe::Answered {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the slot of a connection that ended was never freed"
        );
        thread::sleep(Duration::from_millis(10));
    }

    drop(first);
}

/// `serve` returns within a bounded time after `stop` while a peer holds an idle connection.
///
/// The peer calls once and is answered *before* the stop, which is what makes it a connection the
/// accept loop actually took and is serving. Without that round trip the stop could land before the
/// accept, the connection would never be registered, and this would pass without exercising
/// anything — the vacuous arm being the more likely one, since connecting and stopping are
/// microseconds apart.
///
/// It then sends nothing, which parks its connection thread in a blocking read. That is the whole
/// defect: a `thread::scope` joins every connection thread, so an idle peer held `serve` open for
/// as long as it cared to, and was
/// released the instant the peer was dropped.
#[test]
fn serve_returns_after_stop_while_an_idle_peer_holds_a_connection() {
    let dir = TempDir::new().unwrap();
    let (server, _subscribers) = started(&dir);

    let mut idle = client(&server);
    let answer = idle.call(Method::SystemPing, None).expect("an answer");
    assert!(answer.error.is_none(), "control: the peer is being served");

    let returned = shutdown_within(server);

    assert_eq!(
        returned,
        Ok(()),
        "serve did not return within {BOUND:?} while an idle peer held a connection"
    );
    drop(idle);
}

/// The control for the test above: with no peer at all, `stop` still ends the loop.
///
/// This one passes against the unfixed transport too, and that is its job — it separates "the idle
/// peer is what held it open" from "shutdown is broken in general", and it is what would catch a
/// fix that bounded shutdown by breaking it.
#[test]
fn serve_returns_after_stop_when_no_peer_is_connected() {
    let dir = TempDir::new().unwrap();
    let (server, _subscribers) = started(&dir);

    assert_eq!(
        shutdown_within(server),
        Ok(()),
        "serve did not return within {BOUND:?} with nothing connected"
    );
}

/// A subscriber that stops reading does not prevent `serve` returning after `stop`, within the
/// same bound.
///
/// The wedge is **confirmed, not assumed**. The fan-out thread reports each completed append on a
/// channel, and the test only proceeds once that channel has gone quiet for [`BLOCKED_FOR`] — so a
/// scheduling accident cannot pass for a blocked write, and a run in which nothing wedged fails
/// here rather than going on to assert something it did not set up.
///
/// Two threads are parked at that moment and both are released by the same halt: the connection
/// thread in `read`, which `serve` must join, and the fan-out thread in `write`, which by invariant
/// 1 is the daemon's single write path holding the registry lock. The second assertion is what says
/// the fix reaches the writer as well as the reader — a shutdown that only unblocked the reader
/// would leave the daemon's write path parked for ever and still satisfy the first.
#[test]
fn a_subscriber_that_stops_reading_does_not_hold_serve_open() {
    let dir = TempDir::new().unwrap();
    let (server, subscribers) = started(&dir);
    let wedged = subscribed(&server, &subscribers);

    let (appended, done) = fan_out(&subscribers);
    let mut delivered = 0;
    // A timeout is the wedge and a disconnection is the fan-out having finished all of them; both
    // end the drain, and the assertion below is what tells them apart, on the count rather than on
    // which way the channel ended.
    while let Ok(_lsn) = appended.recv_timeout(BLOCKED_FOR) {
        delivered += 1;
    }
    assert!(
        delivered < APPENDS,
        "all {APPENDS} appends completed: the subscriber did not wedge the fan-out, so this test \
         measured nothing"
    );

    let returned = shutdown_within(server);

    assert_eq!(
        returned,
        Ok(()),
        "serve did not return within {BOUND:?} while a wedged subscriber held a connection"
    );
    assert_eq!(
        done.recv_timeout(BOUND),
        Ok(()),
        "the fan-out was still blocked after serve returned: the halt reached the reader and not \
         the writer"
    );
    drop(wedged);
}

/// A control for a different property — delivery completeness: a subscriber that **does** read
/// wedges nothing, receives what it was sent, and `serve` still returns.
///
/// Without it, a change that dropped every subscriber the moment it was registered would satisfy
/// the wedged arm perfectly. It is **not** the discriminator for the test above: remove the halt
/// loop from `stop` and this goes red alongside the wedged arm, so it cannot tell "the wedged
/// subscriber is what held `serve` open" from "shutdown is broken in general" —
/// `serve_returns_after_stop_when_no_peer_is_connected` is what does that. What this one kills
/// alone is a fan-out that delivers less than it was given.
#[test]
fn a_reading_subscriber_receives_and_does_not_hold_serve_open() {
    let dir = TempDir::new().unwrap();
    let (server, subscribers) = started(&dir);
    let reading = subscribed(&server, &subscribers);

    let drained = {
        let mut stream = reading.try_clone().unwrap();
        thread::spawn(move || {
            let mut sink = std::io::sink();
            std::io::copy(&mut stream, &mut sink).unwrap_or(0)
        })
    };

    let (_appended, done) = fan_out(&subscribers);
    assert_eq!(
        done.recv_timeout(BOUND),
        Ok(()),
        "the fan-out did not finish against a subscriber that was reading"
    );

    assert_eq!(
        shutdown_within(server),
        Ok(()),
        "serve did not return within {BOUND:?} with a reading subscriber connected"
    );
    drop(reading);
    let bytes = drained.join().expect("the drain thread");
    assert!(
        bytes >= APPENDS * PAYLOAD_BYTES as u64,
        "the reading subscriber received {bytes} bytes, fewer than the {APPENDS} payloads it was \
         sent"
    );
}

/// The claim `StopFlag`'s doc calls "the whole of why this is not flaky": a connection accepted but
/// not yet registered when `stop` lands is still halted.
///
/// The peer deliberately sends nothing and is *not* round-tripped, because the round trip in
/// `serve_returns_after_stop_while_an_idle_peer_holds_a_connection` is what puts that test safely
/// past admission — which is exactly the window this one is for. The sleep is what makes it
/// deterministic rather than a race the runner wins most of the time: it puts the accept loop
/// certainly past `accept()` and certainly before `stop()`.
#[test]
fn stop_landing_during_admission_still_bounds_serve() {
    let dir = TempDir::new().unwrap();
    let (server, _subscribers) = started(&dir);
    let peer = UnixStream::connect(server.socket()).expect("connect to the test socket");
    thread::sleep(Duration::from_millis(100));

    let returned = shutdown_within(server);

    assert_eq!(
        returned,
        Ok(()),
        "serve did not return within {BOUND:?} when stop landed during admission"
    );
    drop(peer);
}
