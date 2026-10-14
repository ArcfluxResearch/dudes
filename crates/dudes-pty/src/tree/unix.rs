//! Unix PTY setup: session and controlling terminal before exec, process-group teardown after.

#![allow(unsafe_code)]

#[cfg(test)]
use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::{self, Write as _};
use std::os::unix::process::CommandExt as _;
#[cfg(test)]
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use dudes_providers::launch::LaunchSpec;
use rustix::fd::AsFd;
use rustix::fs::{Mode, OFlags};
use rustix::io::{Errno, FdFlags};
use rustix::process::{Pid, Signal};
use rustix::pty::OpenptFlags;
use rustix::termios::Winsize;

use crate::PtySize;
use crate::tree::{CloseReport, Spawned};

#[cfg(test)]
thread_local! {
    static FORCE_GROUP_ISSUE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static FORCE_STATUS_ERROR: Cell<bool> = const { Cell::new(false) };
    static ABSENCE_QUERY_RESULT: RefCell<Option<io::Result<bool>>> = const { RefCell::new(None) };
}

#[cfg(test)]
const FIRST_HEARTBEAT_TIMEOUT: &str = "PTY_TEST_FIRST_HEARTBEAT_TIMEOUT";
#[cfg(test)]
const FORCED_GROUP_ISSUE: &str = "injected post-spawn group uncertainty";
#[cfg(test)]
const FIRST_HEARTBEAT_WAIT: Duration = Duration::from_secs(2);

pub(crate) struct Session {
    child: Child,
    original_group: Option<Pid>,
    cleanup: CleanupState,
    control: File,
    writer: File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroupCleanup {
    Unknown,
    SignalOpen(Pid),
    ProbeOnly(Pid),
    ConfirmedAbsent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectCleanup {
    NeedsSignal,
    WaitAuthorized,
    ReapUncertain,
    Reaped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CleanupState {
    group: GroupCleanup,
    direct: DirectCleanup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectStatus {
    Running,
    Exited,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroupPhase {
    Unknown,
    SignalOpen,
    ProbeOnly,
    ConfirmedAbsent,
}

impl GroupCleanup {
    fn enter_probe_only(&mut self) {
        if let Self::SignalOpen(group) = *self {
            *self = Self::ProbeOnly(group);
        }
    }

    #[cfg(test)]
    fn phase(self) -> GroupPhase {
        match self {
            Self::Unknown => GroupPhase::Unknown,
            Self::SignalOpen(_) => GroupPhase::SignalOpen,
            Self::ProbeOnly(_) => GroupPhase::ProbeOnly,
            Self::ConfirmedAbsent => GroupPhase::ConfirmedAbsent,
        }
    }
}

pub(crate) fn spawn(spec: LaunchSpec, size: PtySize) -> io::Result<Spawned> {
    let controller = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
    let flags = rustix::io::fcntl_getfd(&controller)? | FdFlags::CLOEXEC;
    rustix::io::fcntl_setfd(&controller, flags)?;
    rustix::pty::grantpt(&controller)?;
    rustix::pty::unlockpt(&controller)?;
    let name = rustix::pty::ptsname(&controller, Vec::new())?;
    let user = rustix::fs::open(
        name,
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    set_size(&controller, size)?;

    let reader = File::from(rustix::io::fcntl_dupfd_cloexec(&controller, 3)?);
    let writer = File::from(rustix::io::fcntl_dupfd_cloexec(&controller, 3)?);
    let control = File::from(controller);
    let user = rustix::io::fcntl_dupfd_cloexec(&user, 3)?;

    let mut command = Command::new(&spec.argv[0]);
    command.args(&spec.argv[1..]);
    command.env_clear();
    for env in spec.env {
        command.env(env.name(), env.expose_value());
    }
    command.current_dir(spec.cwd);
    // SAFETY: the closure runs after fork and before exec. It calls only direct rustix syscall
    // wrappers over owned descriptors: setsid, TIOCSCTTY and dup2. It allocates nothing, locks
    // nothing, touches no shared memory, and retains no borrowed pointer across exec.
    unsafe {
        command.pre_exec(move || {
            rustix::process::setsid().map_err(io::Error::from)?;
            rustix::process::ioctl_tiocsctty(&user).map_err(io::Error::from)?;
            rustix::stdio::dup2_stdin(&user).map_err(io::Error::from)?;
            rustix::stdio::dup2_stdout(&user).map_err(io::Error::from)?;
            rustix::stdio::dup2_stderr(&user).map_err(io::Error::from)?;
            Ok(())
        });
    }

    let child = command.spawn()?;
    let group = pid(child.id()).ok();
    let issue = group_issue(group);
    Ok(Spawned {
        session: Session {
            child,
            original_group: group,
            cleanup: CleanupState {
                group: group.map_or(GroupCleanup::Unknown, GroupCleanup::SignalOpen),
                direct: DirectCleanup::NeedsSignal,
            },
            control,
            writer,
        },
        reader,
        issue,
    })
}

fn group_issue(group: Option<Pid>) -> Option<io::Error> {
    #[cfg(test)]
    if let Some(readiness) = FORCE_GROUP_ISSUE.with(|force| force.borrow_mut().take()) {
        return Some(forced_group_issue_after_readiness(
            &readiness,
            FIRST_HEARTBEAT_WAIT,
        ));
    }
    match group {
        Some(group) if rustix::process::getpgid(Some(group)) == Ok(group) => None,
        Some(_) => Some(io::Error::other(
            "the PTY child process group could not be confirmed after spawn",
        )),
        None => Some(io::Error::new(
            io::ErrorKind::InvalidData,
            "the PTY child pid cannot name a process group",
        )),
    }
}

#[cfg(test)]
pub(crate) fn first_heartbeat(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() != 20 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(&bytes).ok()?.parse().ok()
}

#[cfg(test)]
fn forced_group_issue_after_readiness(path: &Path, timeout: Duration) -> io::Error {
    match wait_for_first_heartbeat(path, timeout) {
        Ok(_) => io::Error::other(FORCED_GROUP_ISSUE),
        Err(error) => error,
    }
}

#[cfg(test)]
fn wait_for_first_heartbeat(path: &Path, timeout: Duration) -> io::Result<u64> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = first_heartbeat(path) {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                FIRST_HEARTBEAT_TIMEOUT,
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
pub(crate) fn force_group_issue_once_for_test(readiness: &Path) {
    FORCE_GROUP_ISSUE.with(|force| {
        assert!(
            force.replace(Some(readiness.to_owned())).is_none(),
            "forced group issue already armed"
        );
    });
}

#[cfg(test)]
pub(crate) fn force_status_error_once_for_test() {
    FORCE_STATUS_ERROR.with(|force| force.set(true));
}

fn child_status(child: &mut Child) -> io::Result<Option<std::process::ExitStatus>> {
    #[cfg(test)]
    if FORCE_STATUS_ERROR.with(|force| force.replace(false)) {
        return Err(io::Error::other(
            "injected child status observation failure",
        ));
    }
    child.try_wait()
}

trait UnixTeardownOps {
    fn signal_group(&mut self, group: Pid) -> io::Result<GroupState>;
    fn signal_direct(&mut self) -> io::Result<()>;
    fn try_wait_direct(&mut self) -> io::Result<DirectStatus>;
    fn wait_direct(&mut self) -> io::Result<()>;
    fn wait_group_absent(&mut self, group: Pid) -> io::Result<()>;
}

struct RealTeardownOps<'a> {
    child: &'a mut Child,
}

impl UnixTeardownOps for RealTeardownOps<'_> {
    fn signal_group(&mut self, group: Pid) -> io::Result<GroupState> {
        signal_group(rustix::process::kill_process_group(group, Signal::KILL))
    }

    fn signal_direct(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    fn try_wait_direct(&mut self) -> io::Result<DirectStatus> {
        child_status(self.child).map(|status| {
            if status.is_some() {
                DirectStatus::Exited
            } else {
                DirectStatus::Running
            }
        })
    }

    fn wait_direct(&mut self) -> io::Result<()> {
        self.child.wait().map(drop)
    }

    fn wait_group_absent(&mut self, group: Pid) -> io::Result<()> {
        wait_group(group)
    }
}

fn remember(slot: &mut Option<io::Error>, error: io::Error) {
    if slot.is_none() {
        *slot = Some(error);
    }
}

fn close_owned(state: &mut CleanupState, ops: &mut impl UnixTeardownOps) -> CloseReport {
    let mut visible_error = None;
    let mut deferred_signal_error = None;

    if let GroupCleanup::SignalOpen(group) = state.group
        && let Err(error) = ops.signal_group(group)
    {
        remember(&mut deferred_signal_error, error);
    } else if state.group == GroupCleanup::Unknown {
        remember(
            &mut visible_error,
            io::Error::other("PTY process group is unknown"),
        );
    }

    if state.direct == DirectCleanup::NeedsSignal
        && matches!(
            state.group,
            GroupCleanup::SignalOpen(_) | GroupCleanup::Unknown
        )
    {
        match ops.signal_direct() {
            Ok(()) => state.direct = DirectCleanup::WaitAuthorized,
            Err(error) => remember(&mut deferred_signal_error, error),
        }
    }

    if matches!(
        state.group,
        GroupCleanup::SignalOpen(_) | GroupCleanup::Unknown
    ) && matches!(
        state.direct,
        DirectCleanup::NeedsSignal | DirectCleanup::WaitAuthorized
    ) {
        match ops.try_wait_direct() {
            Ok(DirectStatus::Exited) => {
                state.direct = DirectCleanup::Reaped;
                state.group.enter_probe_only();
            }
            Ok(DirectStatus::Running) => {
                if state.direct == DirectCleanup::WaitAuthorized {
                    state.group.enter_probe_only();
                }
            }
            Err(error) => {
                remember(&mut visible_error, error);
                state.group.enter_probe_only();
                if state.direct == DirectCleanup::NeedsSignal {
                    state.direct = DirectCleanup::ReapUncertain;
                }
            }
        }
    } else if state.direct == DirectCleanup::ReapUncertain {
        match ops.try_wait_direct() {
            Ok(DirectStatus::Exited) => state.direct = DirectCleanup::Reaped,
            Ok(DirectStatus::Running) => {}
            Err(error) => remember(&mut visible_error, error),
        }
    }

    if state.direct == DirectCleanup::WaitAuthorized
        && !matches!(state.group, GroupCleanup::SignalOpen(_))
    {
        match ops.wait_direct() {
            Ok(()) => state.direct = DirectCleanup::Reaped,
            Err(error) => remember(&mut visible_error, error),
        }
    }

    if state.direct == DirectCleanup::Reaped
        && let GroupCleanup::ProbeOnly(group) = state.group
    {
        match ops.wait_group_absent(group) {
            Ok(()) => state.group = GroupCleanup::ConfirmedAbsent,
            Err(error) => remember(&mut visible_error, error),
        }
    }

    let confirmed =
        state.group == GroupCleanup::ConfirmedAbsent && state.direct == DirectCleanup::Reaped;
    let error = if confirmed {
        visible_error
    } else {
        visible_error
            .or(deferred_signal_error)
            .or_else(|| Some(io::Error::other("PTY cleanup is unconfirmed")))
    };
    CloseReport { confirmed, error }
}

impl Session {
    pub(crate) fn close_query(&self) -> io::Result<CloseQuery> {
        if self.cleanup.direct != DirectCleanup::Reaped {
            return Err(io::Error::other("PTY direct child is not reaped"));
        }
        self.original_group
            .map(|group| CloseQuery { group })
            .ok_or_else(|| io::Error::other("PTY original group is unknown"))
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writer.write(bytes)
    }

    pub(crate) fn resize(&mut self, size: PtySize) -> io::Result<()> {
        set_size(&self.control, size)
    }

    pub(crate) fn close(&mut self) -> CloseReport {
        close_owned(
            &mut self.cleanup,
            &mut RealTeardownOps {
                child: &mut self.child,
            },
        )
    }
}

pub(crate) struct CloseQuery {
    group: Pid,
}

impl CloseQuery {
    pub(crate) fn is_absent(&self) -> io::Result<bool> {
        #[cfg(test)]
        if let Some(result) = ABSENCE_QUERY_RESULT.with(|forced| forced.borrow_mut().take()) {
            return result;
        }
        probe_group(rustix::process::test_kill_process_group(self.group))
            .map(|state| state == GroupState::Absent)
    }
}

#[cfg(test)]
pub(crate) fn force_absence_query_once_for_test(result: io::Result<bool>) {
    ABSENCE_QUERY_RESULT.with(|forced| *forced.borrow_mut() = Some(result));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroupState {
    Present,
    Absent,
}

fn classify_group(result: rustix::io::Result<()>) -> io::Result<GroupState> {
    match result {
        Ok(()) => Ok(GroupState::Present),
        Err(Errno::SRCH) => Ok(GroupState::Absent),
        Err(error) => Err(error.into()),
    }
}

fn signal_group(result: rustix::io::Result<()>) -> io::Result<GroupState> {
    classify_group(result)
}

fn probe_group(result: rustix::io::Result<()>) -> io::Result<GroupState> {
    classify_group(result)
}

fn wait_group(group: Pid) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut last_uncertainty = None;
    loop {
        match probe_group(rustix::process::test_kill_process_group(group)) {
            Ok(GroupState::Absent) => return Ok(()),
            Ok(GroupState::Present) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(GroupState::Present) => {
                return Err(last_uncertainty.unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::TimedOut, "PTY group survived kill")
                }));
            }
            Err(error) if Instant::now() < deadline => {
                last_uncertainty = Some(error);
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(error);
            }
        }
    }
}

fn set_size(fd: impl AsFd, size: PtySize) -> io::Result<()> {
    rustix::termios::tcsetwinsize(
        fd,
        Winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )?;
    Ok(())
}

fn pid(value: u32) -> io::Result<Pid> {
    Pid::from_raw(
        i32::try_from(value).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "child pid does not fit i32")
        })?,
    )
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "child pid is zero"))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn first_heartbeat_requires_one_complete_fixed_width_counter() {
        let temp = tempfile::tempdir().unwrap();
        let beat = temp.path().join("first.beat");
        let mut ledger = vec![("absent", first_heartbeat(&beat))];

        std::fs::write(&beat, b"").unwrap();
        ledger.push(("empty", first_heartbeat(&beat)));
        assert!(
            beat.exists(),
            "the old existence predicate must fire on the empty state"
        );

        std::fs::write(&beat, b"1234567890123456789").unwrap();
        ledger.push(("partial", first_heartbeat(&beat)));
        std::fs::write(&beat, b"0000000000000000000x").unwrap();
        ledger.push(("malformed", first_heartbeat(&beat)));
        std::fs::write(&beat, b"99999999999999999999").unwrap();
        ledger.push(("overflow", first_heartbeat(&beat)));
        std::fs::write(&beat, b"00000000000000000007").unwrap();
        ledger.push(("complete", first_heartbeat(&beat)));

        assert_eq!(
            ledger,
            [
                ("absent", None),
                ("empty", None),
                ("partial", None),
                ("malformed", None),
                ("overflow", None),
                ("complete", Some(7)),
            ]
        );
    }

    #[test]
    fn forced_group_issue_waits_past_the_old_delay_for_a_complete_heartbeat() {
        let temp = tempfile::tempdir().unwrap();
        let beat = temp.path().join("delayed.beat");
        let (empty_send, empty_receive) = mpsc::sync_channel(0);
        let (publish_send, publish_receive) = mpsc::sync_channel(0);
        let publisher_beat = beat.clone();
        let publisher = thread::spawn(move || {
            std::fs::write(&publisher_beat, b"").unwrap();
            empty_send.send(()).unwrap();
            publish_receive.recv().unwrap();
            std::fs::write(&publisher_beat, b"00000000000000000007").unwrap();
        });
        empty_receive
            .recv_timeout(Duration::from_secs(1))
            .expect("publisher reached the empty state");

        let (issue_send, issue_receive) = mpsc::sync_channel(0);
        let waiter_beat = beat.clone();
        let waiter = thread::spawn(move || {
            issue_send
                .send(forced_group_issue_after_readiness(
                    &waiter_beat,
                    Duration::from_secs(2),
                ))
                .unwrap();
        });

        thread::sleep(Duration::from_millis(175));
        assert!(
            beat.exists(),
            "the old existence predicate must fire before publication"
        );
        assert_eq!(first_heartbeat(&beat), None);
        assert!(
            matches!(issue_receive.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "the old fixed delay released the injected issue while the heartbeat was empty"
        );

        publish_send.send(()).unwrap();
        let issue = issue_receive
            .recv_timeout(Duration::from_secs(1))
            .expect("complete heartbeat released the injected issue");
        assert_eq!(issue.to_string(), "injected post-spawn group uncertainty");
        publisher.join().unwrap();
        waiter.join().unwrap();
    }

    #[test]
    fn readiness_timeout_never_releases_the_injected_group_issue() {
        let temp = tempfile::tempdir().unwrap();
        let beat = temp.path().join("absent.beat");
        let (send, receive) = mpsc::sync_channel(0);
        let waiter = thread::spawn(move || {
            send.send(forced_group_issue_after_readiness(
                &beat,
                Duration::from_millis(20),
            ))
            .unwrap();
        });

        let error = receive
            .recv_timeout(Duration::from_secs(1))
            .expect("readiness timeout remained bounded");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "PTY_TEST_FIRST_HEARTBEAT_TIMEOUT");
        assert_ne!(error.to_string(), "injected post-spawn group uncertainty");
        waiter.join().unwrap();
    }

    struct FakeOps {
        group_signals: VecDeque<io::Result<GroupState>>,
        direct_signals: VecDeque<io::Result<()>>,
        statuses: VecDeque<io::Result<DirectStatus>>,
        waits: VecDeque<io::Result<()>>,
        group_probes: VecDeque<io::Result<()>>,
        events: Vec<&'static str>,
        leader_reserved: bool,
    }

    impl FakeOps {
        fn new() -> Self {
            Self {
                group_signals: VecDeque::new(),
                direct_signals: VecDeque::new(),
                statuses: VecDeque::new(),
                waits: VecDeque::new(),
                group_probes: VecDeque::new(),
                events: Vec::new(),
                leader_reserved: true,
            }
        }

        fn count(&self, event: &str) -> usize {
            self.events.iter().filter(|seen| **seen == event).count()
        }
    }

    impl UnixTeardownOps for FakeOps {
        fn signal_group(&mut self, _group: Pid) -> io::Result<GroupState> {
            assert!(
                self.leader_reserved,
                "group signal crossed the reap boundary"
            );
            self.events.push("group_signal");
            self.group_signals.pop_front().expect("group signal script")
        }

        fn signal_direct(&mut self) -> io::Result<()> {
            assert!(
                self.leader_reserved,
                "direct signal crossed the reap boundary"
            );
            self.events.push("direct_signal");
            self.direct_signals
                .pop_front()
                .expect("direct signal script")
        }

        fn try_wait_direct(&mut self) -> io::Result<DirectStatus> {
            self.events.push("try_wait");
            let result = self.statuses.pop_front().expect("status script");
            if !matches!(result, Ok(DirectStatus::Running)) {
                self.leader_reserved = false;
            }
            result
        }

        fn wait_direct(&mut self) -> io::Result<()> {
            self.events.push("wait");
            self.leader_reserved = false;
            self.waits.pop_front().expect("wait script")
        }

        fn wait_group_absent(&mut self, _group: Pid) -> io::Result<()> {
            assert!(
                !self.leader_reserved,
                "group absence was probed before the reap boundary"
            );
            self.events.push("group_probe");
            self.group_probes.pop_front().expect("group probe script")
        }
    }

    fn cleanup_state() -> CleanupState {
        CleanupState {
            group: GroupCleanup::SignalOpen(pid(42).unwrap()),
            direct: DirectCleanup::NeedsSignal,
        }
    }

    fn denied(message: &'static str) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, message)
    }

    fn failed(message: &'static str) -> io::Error {
        io::Error::other(message)
    }

    fn assert_group_signal_guard_fires(ops: &mut FakeOps) {
        let group = pid(42).unwrap();
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = ops.signal_group(group);
            }))
            .is_err(),
            "post-reap group-signal mutant did not fire"
        );
    }

    #[test]
    fn group_result_classifies_success_esrch_eperm_and_other_error() {
        assert_eq!(classify_group(Ok(())).unwrap(), GroupState::Present);
        assert_eq!(
            classify_group(Err(Errno::SRCH)).unwrap(),
            GroupState::Absent
        );
        let permission = classify_group(Err(Errno::PERM)).unwrap_err();
        assert_eq!(permission.kind(), io::ErrorKind::PermissionDenied);
        let other = classify_group(Err(Errno::INVAL)).unwrap_err();
        assert_ne!(other.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn teardown_never_signals_group_after_reap_boundary() {
        let mut exited = FakeOps::new();
        exited.group_signals.push_back(Ok(GroupState::Present));
        exited.direct_signals.push_back(Ok(()));
        exited.statuses.push_back(Ok(DirectStatus::Exited));
        exited.group_probes.push_back(Ok(()));
        assert!(close_owned(&mut cleanup_state(), &mut exited).confirmed);
        assert_group_signal_guard_fires(&mut exited);

        let mut uncertain = FakeOps::new();
        uncertain.group_signals.push_back(Ok(GroupState::Present));
        uncertain.direct_signals.push_back(Err(denied("direct")));
        uncertain.statuses.push_back(Err(failed("status")));
        let mut uncertain_state = cleanup_state();
        assert!(!close_owned(&mut uncertain_state, &mut uncertain).confirmed);
        assert_eq!(uncertain_state.group.phase(), GroupPhase::ProbeOnly);
        assert_group_signal_guard_fires(&mut uncertain);

        let mut waited = FakeOps::new();
        waited.group_signals.push_back(Ok(GroupState::Present));
        waited.direct_signals.push_back(Ok(()));
        waited.statuses.push_back(Ok(DirectStatus::Running));
        waited.waits.push_back(Err(failed("wait")));
        assert!(!close_owned(&mut cleanup_state(), &mut waited).confirmed);
        assert_group_signal_guard_fires(&mut waited);
    }

    #[test]
    fn aggregate_group_success_does_not_authorize_direct_wait() {
        let mut ops = FakeOps::new();
        ops.group_signals.push_back(Ok(GroupState::Present));
        ops.direct_signals.push_back(Err(denied("direct")));
        ops.statuses.push_back(Ok(DirectStatus::Running));
        let mut state = cleanup_state();

        let report = close_owned(&mut state, &mut ops);

        assert!(!report.confirmed);
        assert_eq!(state.group.phase(), GroupPhase::SignalOpen);
        assert_eq!(state.direct, DirectCleanup::NeedsSignal);
        assert_eq!(ops.count("wait"), 0);
        assert_eq!(
            report.error.unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn unreaped_retry_requires_try_wait_none() {
        let mut ops = FakeOps::new();
        ops.group_signals.push_back(Ok(GroupState::Present));
        ops.direct_signals.push_back(Err(denied("first direct")));
        ops.statuses.push_back(Ok(DirectStatus::Running));
        let mut state = cleanup_state();
        assert!(!close_owned(&mut state, &mut ops).confirmed);
        assert_eq!(state.group.phase(), GroupPhase::SignalOpen);
        assert_eq!(state.direct, DirectCleanup::NeedsSignal);

        ops.group_signals.push_back(Ok(GroupState::Present));
        ops.direct_signals.push_back(Ok(()));
        ops.statuses.push_back(Ok(DirectStatus::Running));
        ops.waits.push_back(Ok(()));
        ops.group_probes.push_back(Ok(()));
        assert!(close_owned(&mut state, &mut ops).confirmed);
        assert_eq!(ops.count("group_signal"), 2);
        assert_eq!(ops.count("direct_signal"), 2);
        assert_eq!(ops.count("wait"), 1);
        assert_eq!(ops.count("group_probe"), 1);
    }

    #[test]
    fn post_reap_probe_uncertainty_retries_without_signalling() {
        let mut ops = FakeOps::new();
        ops.group_signals.push_back(Ok(GroupState::Present));
        ops.direct_signals.push_back(Ok(()));
        ops.statuses.push_back(Ok(DirectStatus::Exited));
        ops.group_probes.push_back(Err(failed("probe timeout")));
        let mut state = cleanup_state();
        assert!(!close_owned(&mut state, &mut ops).confirmed);
        assert_eq!(state.group.phase(), GroupPhase::ProbeOnly);
        assert_eq!(state.direct, DirectCleanup::Reaped);

        let signal_count = ops.count("group_signal");
        let direct_count = ops.count("direct_signal");
        ops.group_probes.push_back(Ok(()));
        assert!(close_owned(&mut state, &mut ops).confirmed);
        assert_eq!(ops.count("group_signal"), signal_count);
        assert_eq!(ops.count("direct_signal"), direct_count);
        assert_eq!(ops.count("group_probe"), 2);
    }

    #[test]
    fn natural_exit_confirms_after_reap_and_suppresses_stale_signal_errors() {
        let mut ops = FakeOps::new();
        ops.group_signals.push_back(Err(failed("group gone")));
        ops.direct_signals.push_back(Err(failed("direct gone")));
        ops.statuses.push_back(Ok(DirectStatus::Exited));
        ops.group_probes.push_back(Ok(()));
        let report = close_owned(&mut cleanup_state(), &mut ops);

        assert!(report.confirmed);
        assert!(report.error.is_none(), "stale target error was returned");
    }

    #[test]
    fn failed_wait_retries_without_another_numeric_signal() {
        let mut ops = FakeOps::new();
        ops.group_signals.push_back(Ok(GroupState::Present));
        ops.direct_signals.push_back(Ok(()));
        ops.statuses.push_back(Ok(DirectStatus::Running));
        ops.waits.push_back(Err(failed("wait")));
        let mut state = cleanup_state();
        assert!(!close_owned(&mut state, &mut ops).confirmed);
        assert_eq!(state.group.phase(), GroupPhase::ProbeOnly);
        assert_eq!(state.direct, DirectCleanup::WaitAuthorized);

        let group_signals = ops.count("group_signal");
        let direct_signals = ops.count("direct_signal");
        ops.waits.push_back(Ok(()));
        ops.group_probes.push_back(Ok(()));
        assert!(close_owned(&mut state, &mut ops).confirmed);
        assert_eq!(ops.count("group_signal"), group_signals);
        assert_eq!(ops.count("direct_signal"), direct_signals);
        assert_eq!(ops.count("wait"), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_zombie_group_probe_becomes_absent_only_after_reap() {
        const ZOMBIE_HELPER: &str = "DUDES_PTY_ZOMBIE_GROUP_HELPER";
        let executable = std::env::current_exe().unwrap();
        let spec = LaunchSpec {
            argv: vec![
                executable.into_os_string(),
                "--ignored".into(),
                "--exact".into(),
                "tree::platform::tests::linux_zombie_group_child".into(),
                "--nocapture".into(),
            ],
            env: vec![dudes_providers::launch::EnvVar::new(ZOMBIE_HELPER, "1")],
            cwd: std::env::current_dir().unwrap(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        };
        let Spawned {
            mut session,
            reader,
            issue,
        } = spawn(spec, PtySize { cols: 80, rows: 24 }).unwrap();
        drop(reader);
        assert!(issue.is_none(), "group setup issue: {issue:?}");
        let child = session.child.id();
        let GroupCleanup::SignalOpen(group) = session.cleanup.group else {
            panic!("spawned group was not signal-open")
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while linux_proc_state(child).as_deref() != Some("Z") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(linux_proc_state(child).as_deref(), Some("Z"));
        assert_ne!(
            probe_group(rustix::process::test_kill_process_group(group)).unwrap(),
            GroupState::Absent
        );
        session.child.wait().unwrap();
        assert_eq!(
            probe_group(rustix::process::test_kill_process_group(group)).unwrap(),
            GroupState::Absent
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "spawned by the Linux zombie process-group control"]
    fn linux_zombie_group_child() {
        assert!(std::env::var_os("DUDES_PTY_ZOMBIE_GROUP_HELPER").is_some());
    }

    #[cfg(target_os = "linux")]
    fn linux_proc_state(pid: u32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        Some(
            stat.get(stat.rfind(')')? + 2..)?
                .chars()
                .next()?
                .to_string(),
        )
    }
}
