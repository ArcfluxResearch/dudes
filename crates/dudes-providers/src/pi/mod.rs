//! Pi in managed mode: one `pi --mode json` process per turn, its task delivered at launch.
//!
//! [`managed::PiAdapter`] composes the launch and [`stream::TurnStream`] reads the run's JSON
//! Lines. Pi names the session on the run's first line, a `session` header, so the id arrives at
//! the start of the turn and the acquisition mode is [`SessionAcquisition::Assigned`]. Pi 0.85.1's `--session-id <id>` also creates a session under an id the caller
//! names; the declaration stays `Assigned`.

pub mod config;
pub mod managed;
pub mod models;
mod steps;
pub mod stream;

pub use managed::PiAdapter;

use crate::adapter::{Arrival, SessionAcquisition};

/// Pi names its own session id, and a later turn resumes it with `--session <id>`.
pub const SESSION_ACQUISITION: SessionAcquisition = SessionAcquisition::Assigned {
    resume: "--session",
    arrival: Arrival::Start,
};

/// Exact Pi releases verified to honor the declared-server extension's configuration source.
pub const PI_MCP_TESTED_VERSIONS: &[&str] = &["1.0.4"];

/// Whether bounded version output names exactly one tested release, without surrounding text.
#[must_use]
pub fn tested_mcp_version(output: &[u8]) -> bool {
    std::str::from_utf8(output).is_ok_and(|output| {
        let mut lines = output.lines();
        lines
            .next()
            .is_some_and(|line| PI_MCP_TESTED_VERSIONS.contains(&line))
            && lines.next().is_none()
    })
}

#[cfg(test)]
mod tests {
    use super::tested_mcp_version;

    #[test]
    fn only_one_exact_tested_version_line_is_accepted() {
        for output in [b"1.0.4".as_slice(), b"1.0.4\n", b"1.0.4\r\n"] {
            assert!(tested_mcp_version(output));
        }
        for output in [
            b"".as_slice(),
            b"1.0.3",
            b"1.0.5",
            b"2.0.0",
            b"1.0.4-rc.1",
            b"v1.0.4",
            b" 1.0.4",
            b"1.0.4 ",
            b"1.0.4\n\n",
            b"1.0.4\nother",
            b"1.0.4\xff",
        ] {
            assert!(!tested_mcp_version(output), "{output:?}");
        }
    }
}
