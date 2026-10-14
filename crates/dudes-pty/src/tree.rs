//! Platform-owned PTY creation and whole-tree teardown.

use std::fs::File;
use std::io::{self, Read as _};

use dudes_providers::launch::LaunchSpec;

use crate::PtySize;

#[cfg(unix)]
#[path = "tree/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "tree/windows.rs"]
mod platform;

pub(crate) use platform::{CloseQuery, Session};

pub(crate) struct Spawned {
    pub(crate) session: Session,
    pub(crate) reader: File,
    /// A failure observed after the child existed. The session remains owned and must be cleaned.
    pub(crate) issue: Option<io::Error>,
}

/// Native teardown evidence. Only `confirmed` permits the host map entry to be removed.
pub(crate) struct CloseReport {
    pub(crate) confirmed: bool,
    pub(crate) error: Option<io::Error>,
}

pub(crate) fn spawn(spec: LaunchSpec, size: PtySize) -> io::Result<Spawned> {
    platform::spawn(spec, size)
}

pub(crate) fn read(reader: &mut File, bytes: &mut [u8]) -> io::Result<usize> {
    match reader.read(bytes) {
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) => Ok(0),
        result => result,
    }
}

#[cfg(all(test, unix))]
pub(crate) fn force_group_issue_once_for_test(readiness: &std::path::Path) {
    platform::force_group_issue_once_for_test(readiness);
}

#[cfg(all(test, unix))]
pub(crate) fn first_heartbeat_for_test(path: &std::path::Path) -> Option<u64> {
    platform::first_heartbeat(path)
}

#[cfg(all(test, unix))]
pub(crate) fn force_status_error_once_for_test() {
    platform::force_status_error_once_for_test();
}

#[cfg(all(test, unix))]
pub(crate) fn force_absence_query_once_for_test(result: io::Result<bool>) {
    platform::force_absence_query_once_for_test(result);
}
