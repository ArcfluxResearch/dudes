//! What `uninstall` and `upgrade` share about a daemon that may be running.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use dudes_proto::AgentView;
use dudes_proto::rpc::closing::CLOSING_GRACE_SECONDS;
use dudes_types::AgentStatus;

use crate::env::LOCK_NAME;

/// The daemon, reached through the operator door.
pub(crate) trait Daemon {
    /// Whether a daemon answers.
    fn running(&self) -> bool;
    /// The agents that are busy, each described by [`agents_busy`]. An error means the operator
    /// call was refused or failed, and the command must stop.
    fn busy_agents(&self) -> Result<Vec<String>, String>;
    /// Stop it the way `dudes stop` does: closing time.
    fn stop(&self, out: &mut dyn Write, err: &mut dyn Write) -> Result<(), String>;
}

/// The agents a restart would interrupt: running a turn, waiting on a question, or attached to a
/// person's live session. The same three the roster groups as working.
pub(crate) fn agents_busy(agents: &[AgentView]) -> Vec<String> {
    agents
        .iter()
        .filter_map(|agent| {
            let state = match agent.status {
                AgentStatus::Running => "running a turn",
                AgentStatus::Waiting => "waiting for an answer",
                AgentStatus::Attached => "attached to a person's live session",
                AgentStatus::Idle | AgentStatus::Done | AgentStatus::Exhausted => return None,
            };
            Some(format!("{} ({}) is {state}", agent.name, agent.id))
        })
        .collect()
}

/// What `--now` means, said where it is used.
pub(crate) fn closing_note() -> String {
    format!(
        "Closing time: every agent gets up to {} minutes to save its work before the daemon stops.",
        CLOSING_GRACE_SECONDS / 60
    )
}

/// The refusal that names each busy agent and the ways forward.
pub(crate) fn busy_refusal(busy: &[String]) -> String {
    format!(
        "these agents are busy: {}. Wait for them to finish, pass --now to stop with closing time (every agent gets up to {} minutes to save its work), or run dudes stop --hard first to end their turns at once. Nothing was changed.",
        busy.join("; "),
        CLOSING_GRACE_SECONDS / 60
    )
}

/// The first step with a running daemon: the operator-authorized busy-agent call. `Ok(Some(_))` is
/// the refusal to print; an `Err` means the call itself failed, and nothing may proceed.
pub(crate) fn check_busy(
    daemon: &dyn Daemon,
    now: bool,
    out: &mut dyn Write,
) -> Result<Option<String>, String> {
    let busy = daemon.busy_agents().map_err(|reason| {
        format!("could not ask the daemon which agents are busy ({reason}); nothing was changed")
    })?;
    if busy.is_empty() {
        return Ok(None);
    }
    if !now {
        return Ok(Some(busy_refusal(&busy)));
    }
    let _ = writeln!(out, "{}", closing_note());
    Ok(None)
}

/// Whether another process holds the daemon's lock in `state_dir`.
pub(crate) fn lock_held(state_dir: &Path) -> bool {
    std::fs::File::open(state_dir.join(LOCK_NAME)).is_ok_and(|lock| {
        matches!(
            lock.try_lock_shared(),
            Err(std::fs::TryLockError::WouldBlock)
        )
    })
}

/// Wait up to `wait` for the lock to be released. Returns whether it is free.
pub(crate) fn wait_for_unlock(state_dir: &Path, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    while lock_held(state_dir) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;
    use std::path::PathBuf;

    use super::*;

    /// A daemon for the commands' tests.
    #[derive(Default)]
    #[expect(
        clippy::struct_excessive_bools,
        reason = "one switch per call the fake can fail"
    )]
    pub(crate) struct Fake {
        pub(crate) running: bool,
        pub(crate) busy: Vec<String>,
        pub(crate) busy_fails: bool,
        pub(crate) stop_fails: bool,
        pub(crate) listed: Vec<PathBuf>,
        pub(crate) list_fails: bool,
        pub(crate) gone_after_stop: bool,
        pub(crate) plant_on_stop: Option<PathBuf>,
        pub(crate) events: RefCell<Vec<String>>,
    }

    impl Daemon for Fake {
        fn running(&self) -> bool {
            self.running
        }

        fn busy_agents(&self) -> Result<Vec<String>, String> {
            self.events.borrow_mut().push("busy".to_owned());
            if self.busy_fails {
                return Err("unauthorized".to_owned());
            }
            Ok(self.busy.clone())
        }

        fn stop(&self, out: &mut dyn Write, _: &mut dyn Write) -> Result<(), String> {
            self.events.borrow_mut().push("stop".to_owned());
            if self.stop_fails {
                return Err("it would not stop".to_owned());
            }
            if let Some(tree) = &self.plant_on_stop {
                std::fs::create_dir_all(tree).map_err(|error| error.to_string())?;
            }
            writeln!(out, "the daemon stopped").map_err(|error| error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn agent(id: &str, name: &str, status: &str) -> AgentView {
        serde_json::from_value(json!({
            "id": id, "name": name, "role": "engineer", "department": "engineering",
            "provider": "claude", "status": status, "mode": "managed",
        }))
        .expect("one projected agent")
    }

    #[test]
    fn running_waiting_and_attached_agents_are_busy_and_the_rest_are_not() {
        let agents = [
            agent("01JBQ8XK2M3N4P5Q6R7S8T9VWX", "Ada", "running"),
            agent("01ARZ3NDEKTSV4RRFFQ69G5FAV", "Bob", "waiting"),
            agent("01ARZ3NDEKTSV4RRFFQ69G5FAW", "Cy", "attached"),
            agent("01ARZ3NDEKTSV4RRFFQ69G5FAX", "Dee", "idle"),
            agent("01ARZ3NDEKTSV4RRFFQ69G5FAY", "Eve", "done"),
            agent("01ARZ3NDEKTSV4RRFFQ69G5FAZ", "Fay", "exhausted"),
        ];
        assert_eq!(
            agents_busy(&agents),
            [
                "Ada (01JBQ8XK2M3N4P5Q6R7S8T9VWX) is running a turn",
                "Bob (01ARZ3NDEKTSV4RRFFQ69G5FAV) is waiting for an answer",
                "Cy (01ARZ3NDEKTSV4RRFFQ69G5FAW) is attached to a person's live session",
            ]
        );
    }

    #[test]
    fn the_closing_note_says_six_minutes() {
        assert_eq!(
            closing_note(),
            "Closing time: every agent gets up to 6 minutes to save its work before the daemon stops."
        );
    }
}
