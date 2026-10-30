use std::collections::VecDeque;
use std::io::{self, Write as _};
use std::process::ChildStdin;
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use dudes_providers::protocol::{ProtocolAction, StartupFrame};
use dudes_supervisor::host::AdoptionKey;

use crate::ChildStreams;

const MAX_BUFFERED_FRAMES: usize = 4;
const MAX_WRITES: usize = 8;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// A bounded input queue for one owned protocol child.
///
/// Clones share admission state. Sending never waits for pipe I/O or queue capacity.
#[derive(Clone)]
pub struct ProtocolWriter(Arc<Inner>);

impl std::fmt::Debug for ProtocolWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProtocolWriter").finish_non_exhaustive()
    }
}

struct Inner {
    key: AdoptionKey,
    streams: Weak<dyn ChildStreams>,
    state: Mutex<State>,
    changed: Condvar,
    close_timeout: Duration,
}

struct State {
    phase: Phase,
    frames: VecDeque<StartupFrame>,
    writes: usize,
    writing: bool,
    reaped: bool,
}

enum Phase {
    Starting {
        deadline: Instant,
        task_queued: bool,
    },
    Running,
    Closing {
        deadline: Instant,
    },
    Failed,
    Ended,
}

impl State {
    fn terminal(&self) -> bool {
        matches!(self.phase, Phase::Failed | Phase::Ended)
    }

    fn fail(&mut self) {
        self.phase = Phase::Failed;
        self.frames.clear();
    }

    fn deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Starting { deadline, .. } | Phase::Closing { deadline } => Some(deadline),
            Phase::Running | Phase::Failed | Phase::Ended => None,
        }
    }

    fn admit(&mut self, action: ProtocolAction, close_timeout: Duration) -> io::Result<()> {
        if self.terminal() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "protocol input is closed",
            ));
        }
        if !self.reaped
            && self
                .deadline()
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "protocol deadline expired",
            ));
        }
        match action {
            ProtocolAction::Write(frame) => {
                if self.reaped {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "protocol child was reaped",
                    ));
                }
                let Phase::Starting { task_queued, .. } = &mut self.phase else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "protocol startup input is complete",
                    ));
                };
                if *task_queued || self.writes == MAX_WRITES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "protocol startup write limit exceeded",
                    ));
                }
                if self.frames.len() + usize::from(self.writing) == MAX_BUFFERED_FRAMES {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "protocol input queue is full",
                    ));
                }
                *task_queued = frame.is_task();
                self.writes += 1;
                self.frames.push_back(frame);
            }
            ProtocolAction::Started => {
                if !matches!(
                    self.phase,
                    Phase::Starting {
                        task_queued: true,
                        ..
                    }
                ) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "protocol task was not queued",
                    ));
                }
                self.phase = Phase::Running;
            }
            ProtocolAction::Close => match self.phase {
                Phase::Running => {
                    self.phase = Phase::Closing {
                        deadline: Instant::now() + close_timeout,
                    }
                }
                Phase::Closing { .. } => {}
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "protocol task has not started",
                    ));
                }
            },
            ProtocolAction::Fail => self.fail(),
        }
        Ok(())
    }
}

impl ProtocolWriter {
    pub(crate) fn prepare(
        key: AdoptionKey,
        streams: &Arc<dyn ChildStreams>,
        launched_at: Instant,
    ) -> Self {
        Self::with_timeouts(key, streams, launched_at, STARTUP_TIMEOUT, CLOSE_TIMEOUT)
    }

    fn with_timeouts(
        key: AdoptionKey,
        streams: &Arc<dyn ChildStreams>,
        launched_at: Instant,
        startup_timeout: Duration,
        close_timeout: Duration,
    ) -> Self {
        Self(Arc::new(Inner {
            key,
            streams: Arc::downgrade(streams),
            state: Mutex::new(State {
                phase: Phase::Starting {
                    deadline: launched_at + startup_timeout,
                    task_queued: false,
                },
                frames: VecDeque::new(),
                writes: 0,
                writing: false,
                reaped: false,
            }),
            changed: Condvar::new(),
            close_timeout,
        }))
    }

    /// Admit one action without waiting for the child to consume input.
    /// Invalid actions and a full queue fail this protocol and notify its owner.
    pub fn send(&self, action: ProtocolAction) -> io::Result<()> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        let was_terminal = state.terminal();
        let result = state.admit(action, self.0.close_timeout);
        if result.is_err() && !was_terminal {
            state.fail();
        }
        let notify = !was_terminal && matches!(state.phase, Phase::Failed);
        self.0.changed.notify_all();
        drop(state);
        if notify {
            self.notify_failure();
        }
        result
    }

    fn notify_failure(&self) {
        if let Some(streams) = self.0.streams.upgrade() {
            streams.protocol_failed(&self.0.key);
        }
    }

    pub(crate) fn start(&self, pipe: ChildStdin) -> io::Result<()> {
        if self
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .terminal()
        {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "protocol failed before activation",
            ));
        }
        let watcher = self.clone();
        if let Err(error) = thread::Builder::new()
            .name("dudes-protocol-deadline".into())
            .spawn(move || watcher.watch_deadline())
        {
            self.fail();
            return Err(error);
        }
        let writer = self.clone();
        if let Err(error) = thread::Builder::new()
            .name("dudes-protocol-stdin".into())
            .spawn(move || writer.write(pipe))
        {
            self.fail();
            return Err(error);
        }
        Ok(())
    }

    /// Whether the stream has ended or failed, so dropping the row can no longer fail a turn.
    pub(crate) fn is_terminal(&self) -> bool {
        self.0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .terminal()
    }

    pub(crate) fn fail(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.terminal() {
            return;
        }
        state.fail();
        self.0.changed.notify_all();
        drop(state);
        self.notify_failure();
    }

    pub(crate) fn stdout_closed(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.terminal() || matches!(state.phase, Phase::Closing { .. }) {
            return;
        }
        state.fail();
        self.0.changed.notify_all();
        drop(state);
        self.notify_failure();
    }

    pub(crate) fn reaped(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.reaped = true;
        state.frames.clear();
        self.0.changed.notify_all();
    }

    pub(crate) fn finished(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(state.phase, Phase::Ended) {
            return;
        }
        let failed = !matches!(state.phase, Phase::Closing { .. });
        state.phase = Phase::Ended;
        state.frames.clear();
        self.0.changed.notify_all();
        drop(state);
        if failed {
            self.notify_failure();
        }
    }

    fn write(&self, mut pipe: ChildStdin) {
        loop {
            let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
            state = self
                .0
                .changed
                .wait_while(state, |state| {
                    !state.reaped
                        && state.frames.is_empty()
                        && matches!(state.phase, Phase::Starting { .. } | Phase::Running)
                })
                .unwrap_or_else(PoisonError::into_inner);
            if state.terminal() || state.reaped {
                return;
            }
            let Some(frame) = state.frames.pop_front() else {
                return;
            };
            state.writing = true;
            drop(state);
            let result = pipe.write_all(frame.bytes());
            drop(frame);
            let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.writing = false;
            self.0.changed.notify_all();
            drop(state);
            if result.is_err() {
                self.fail();
                return;
            }
        }
    }

    fn watch_deadline(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if state.terminal() || state.reaped {
                return;
            }
            if let Some(deadline) = state.deadline() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    state.fail();
                    self.0.changed.notify_all();
                    drop(state);
                    self.notify_failure();
                    return;
                }
                let (next, _) = self
                    .0
                    .changed
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(PoisonError::into_inner);
                state = next;
            } else {
                state = self
                    .0
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dudes_providers::adapter::{Adapter as _, TurnSession};
    use dudes_providers::codex::app_server::AppServerReader;
    use dudes_providers::codex::managed::CodexAdapter;
    use dudes_providers::codex::startup::CodexStartup;
    use dudes_providers::stream::TurnReader as _;

    struct Sink(Arc<Mutex<Vec<AdoptionKey>>>);
    impl ChildStreams for Sink {
        fn watch(&self, _key: Option<&AdoptionKey>) -> Box<dyn crate::ChildStream> {
            panic!("no process in admission fixture");
        }
        fn protocol_failed(&self, key: &AdoptionKey) {
            self.0.lock().unwrap().push(key.clone());
        }
    }

    struct Fixture {
        writer: ProtocolWriter,
        _sink: Arc<dyn ChildStreams>,
        failures: Arc<Mutex<Vec<AdoptionKey>>>,
    }

    fn fixture(startup: Duration, close: Duration) -> Fixture {
        let failures = Arc::new(Mutex::new(Vec::new()));
        let sink: Arc<dyn ChildStreams> = Arc::new(Sink(Arc::clone(&failures)));
        let key = AdoptionKey {
            pid: 42,
            start_time: 17,
            boot: Some("test-boot".into()),
        };
        Fixture {
            writer: ProtocolWriter::with_timeouts(key, &sink, Instant::now(), startup, close),
            _sink: sink,
            failures,
        }
    }

    fn task_frame() -> StartupFrame {
        let mut reader = AppServerReader::new(CodexStartup::new(
            "/home".into(),
            "/agent".into(),
            "/repo".into(),
            "private-task".into(),
            TurnSession::Assigned,
            CodexAdapter::default().model_selection(),
        ));
        reader.start();
        reader
            .line(r#"{"id":1,"result":{"userAgent":"dudes/0.160.0 (test)","codexHome":"/home"}}"#);
        let response = if cfg!(any(target_os = "macos", target_os = "linux")) {
            r#"{"id":2,"result":{"thread":{"id":"00000000-0000-4000-8000-000000000001","status":{"type":"idle"}},"cwd":"/repo","approvalPolicy":"never","sandbox":{"type":"workspaceWrite","writableRoots":["/agent/tmp","/agent/outbox"],"networkAccess":false,"excludeTmpdirEnvVar":true,"excludeSlashTmp":true}}}"#
        } else {
            r#"{"id":2,"result":{"thread":{"id":"00000000-0000-4000-8000-000000000001","status":{"type":"idle"}},"cwd":"/repo","approvalPolicy":"never","sandbox":{"type":"workspaceWrite","writableRoots":["/agent/tmp"],"networkAccess":false,"excludeTmpdirEnvVar":true,"excludeSlashTmp":true}}}"#
        };
        let read = reader.line(response);
        let ProtocolAction::Write(frame) = read.actions.into_iter().next().unwrap() else {
            panic!("provider did not compose task frame");
        };
        assert!(frame.is_task());
        frame
    }

    #[test]
    fn a_full_queue_fails_without_waiting_and_discards_queued_frames() {
        let fixture = fixture(STARTUP_TIMEOUT, CLOSE_TIMEOUT);
        for _ in 0..4 {
            fixture
                .writer
                .send(ProtocolAction::Write(CodexStartup::initialize()))
                .unwrap();
        }
        assert_eq!(
            fixture
                .writer
                .send(ProtocolAction::Write(CodexStartup::initialize()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(fixture.writer.0.state.lock().unwrap().frames.is_empty());
        let failures = fixture.failures.lock().unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(
            (
                failures[0].pid,
                failures[0].start_time,
                failures[0].boot.as_deref()
            ),
            (42, 17, Some("test-boot"))
        );
        assert_eq!(
            fixture
                .writer
                .send(ProtocolAction::Started)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn one_task_is_admitted_and_no_write_follows_it() {
        for after_started in [false, true] {
            let fixture = fixture(STARTUP_TIMEOUT, CLOSE_TIMEOUT);
            fixture
                .writer
                .send(ProtocolAction::Write(task_frame()))
                .unwrap();
            if after_started {
                fixture.writer.send(ProtocolAction::Started).unwrap();
            }
            assert_eq!(
                fixture
                    .writer
                    .send(ProtocolAction::Write(task_frame()))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(fixture.failures.lock().unwrap().len(), 1);
            assert!(!format!("{:?}", fixture.writer).contains("private-task"));
        }
    }

    #[test]
    fn acknowledgements_cannot_start_or_finish_an_unsubmitted_task() {
        for action in [ProtocolAction::Started, ProtocolAction::Close] {
            let fixture = fixture(STARTUP_TIMEOUT, CLOSE_TIMEOUT);
            assert_eq!(
                fixture.writer.send(action).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(fixture.failures.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn total_write_limit_survives_queue_drain() {
        let fixture = fixture(STARTUP_TIMEOUT, CLOSE_TIMEOUT);
        for _ in 0..8 {
            fixture
                .writer
                .send(ProtocolAction::Write(CodexStartup::initialize()))
                .unwrap();
            assert!(
                fixture
                    .writer
                    .0
                    .state
                    .lock()
                    .unwrap()
                    .frames
                    .pop_front()
                    .is_some()
            );
        }
        assert_eq!(
            fixture
                .writer
                .send(ProtocolAction::Write(CodexStartup::initialize()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(fixture.failures.lock().unwrap().len(), 1);
    }

    #[test]
    fn startup_and_close_deadlines_fail_the_protocol() {
        for closing in [false, true] {
            let fixture = fixture(
                if closing {
                    STARTUP_TIMEOUT
                } else {
                    Duration::from_millis(20)
                },
                Duration::from_millis(20),
            );
            if closing {
                fixture
                    .writer
                    .send(ProtocolAction::Write(task_frame()))
                    .unwrap();
                fixture.writer.send(ProtocolAction::Started).unwrap();
                fixture.writer.send(ProtocolAction::Close).unwrap();
            }
            let watcher = fixture.writer.clone();
            thread::spawn(move || watcher.watch_deadline())
                .join()
                .unwrap();
            assert_eq!(fixture.failures.lock().unwrap().len(), 1);
            assert_eq!(
                fixture
                    .writer
                    .send(ProtocolAction::Write(CodexStartup::initialize()))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::BrokenPipe
            );
            fixture.writer.finished();
            assert_eq!(
                fixture.failures.lock().unwrap().len(),
                2,
                "failure is published again before end delivery"
            );
        }
    }

    #[test]
    fn reap_cancels_deadlines_but_allows_buffered_terminal_acknowledgements() {
        let fixture = fixture(STARTUP_TIMEOUT, Duration::from_millis(20));
        fixture
            .writer
            .send(ProtocolAction::Write(task_frame()))
            .unwrap();
        fixture.writer.reaped();
        let watcher = fixture.writer.clone();
        thread::spawn(move || watcher.watch_deadline())
            .join()
            .unwrap();
        fixture.writer.send(ProtocolAction::Started).unwrap();
        fixture.writer.send(ProtocolAction::Close).unwrap();
        fixture.writer.stdout_closed();
        fixture.writer.finished();
        assert!(fixture.failures.lock().unwrap().is_empty());
    }
}
