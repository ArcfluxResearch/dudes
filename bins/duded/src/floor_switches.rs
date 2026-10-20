//! The Floor's switches as one shared, swappable snapshot.
//!
//! The daemon reads `[floor.*]` from the operator file at start and again on every applied
//! `floor.enrollment.reload`. The gateway reads the snapshot on each request and the loop thread
//! reads it again when a queued Floor job runs, so a switch turned off after a job was queued
//! still refuses that job. Only the loop thread swaps it.

use std::sync::{Arc, PoisonError, RwLock};

use crate::config::OperatorConfig;
use crate::settings::{FloorControl, Settings};

/// The reason a Floor job reports when its switch went off after it was queued.
pub const ACTION_OFF: &str = "action_off";

/// Every `[floor.*]` switch at one moment. Chat has no switch and is not here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is one independent `[floor.*]` operator switch"
)]
pub struct Switches {
    /// `[floor.answer]`.
    pub answer: bool,
    /// `[floor.hire]`.
    pub hire: bool,
    /// `[floor.control]`, `[floor.goal]`, `[floor.retire]` and `[floor.renew]`.
    pub control: FloorControl,
    /// `[floor.recovery]` alone. Recovery is served only with pause and resume: see
    /// [`Self::serves_recovery`].
    pub recovery: bool,
    /// `[floor.board]`.
    pub board: bool,
    /// `[floor.terminal]`. A daemon started with the terminal off cannot turn it on without a
    /// restart, so this is the file's value only while the daemon holds terminal grants.
    pub terminal: bool,
    /// `[floor.git]`.
    pub git: bool,
    /// `[floor.diff]` alone. The diff route is served only with the Git reads: see
    /// [`Self::serves_diff`].
    pub diff: bool,
    /// `[floor.project]`.
    pub project: bool,
}

impl Switches {
    /// What the operator file says. The terminal is the file's value here; the loop thread limits
    /// it to what the run can serve.
    #[must_use]
    pub const fn from_operator(operator: &OperatorConfig) -> Self {
        Self {
            answer: operator.answer_enabled,
            hire: operator.hire_enabled,
            control: operator.control,
            recovery: operator.recovery_enabled,
            board: operator.board_enabled,
            terminal: operator.terminal_enabled,
            git: operator.git_enabled,
            diff: operator.diff_enabled,
            project: operator.project_enabled,
        }
    }

    /// What the daemon was started with.
    #[must_use]
    pub const fn from_settings(settings: &Settings) -> Self {
        Self {
            answer: settings.floor_answer_enabled,
            hire: settings.floor_hire_enabled,
            control: settings.floor_control,
            recovery: settings.floor_recovery_enabled,
            board: settings.floor_board_enabled,
            terminal: settings.floor_terminal_enabled,
            git: settings.floor_git_enabled,
            diff: settings.floor_diff_enabled,
            project: settings.floor_project_enabled,
        }
    }

    /// Recovery needs pause and resume.
    #[must_use]
    pub const fn serves_recovery(self) -> bool {
        self.recovery && self.control.pause_resume()
    }

    /// The diff route needs the Git reads.
    #[must_use]
    pub const fn serves_diff(self) -> bool {
        self.git && self.diff
    }

    /// The switches that differ, by the `dudes config` key that sets each, in a fixed order.
    #[must_use]
    pub fn differing(self, other: Self) -> Vec<&'static str> {
        [
            ("floor.answer", self.answer != other.answer),
            ("floor.hire", self.hire != other.hire),
            (
                "floor.control",
                self.control.pause_resume() != other.control.pause_resume(),
            ),
            ("floor.goal", self.control.goal() != other.control.goal()),
            (
                "floor.retire",
                self.control.retire() != other.control.retire(),
            ),
            ("floor.renew", self.control.renew() != other.control.renew()),
            ("floor.recovery", self.recovery != other.recovery),
            ("floor.board", self.board != other.board),
            ("floor.terminal", self.terminal != other.terminal),
            ("floor.git", self.git != other.git),
            ("floor.diff", self.diff != other.diff),
            ("floor.project", self.project != other.project),
        ]
        .into_iter()
        .filter(|(_, differs)| *differs)
        .map(|(name, _)| name)
        .collect()
    }
}

/// The snapshot the gateway and the loop thread share.
#[derive(Clone, Debug, Default)]
pub struct SharedSwitches(Arc<RwLock<Switches>>);

impl SharedSwitches {
    /// A snapshot holding `switches`.
    #[must_use]
    pub fn new(switches: Switches) -> Self {
        Self(Arc::new(RwLock::new(switches)))
    }

    /// The switches as they are now.
    #[must_use]
    pub fn load(&self) -> Switches {
        *self.0.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replace the snapshot. Only the loop thread calls this, from an applied reload.
    pub fn store(&self, switches: Switches) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = switches;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;

    use dudes_supervisor::testing::FakeClock;

    use super::*;
    use crate::floor_retire::{RetireOutcome, RetireRequest};
    use crate::floor_session::FloorSessions;
    use crate::ports::Job;
    use crate::runtime::refuse_action_off;

    const HOST: &str = "127.0.0.1:43129";
    const ORIGIN: &str = "http://127.0.0.1:43129";

    fn retire_job() -> (
        Job,
        mpsc::Receiver<Result<RetireOutcome, crate::floor_retire::RetireRefusal>>,
    ) {
        let sessions = FloorSessions::new(
            HOST.parse().expect("loopback"),
            Arc::new(FakeClock::frozen(0)),
        );
        let issued = sessions.issue().expect("bootstrap");
        let exchange = sessions
            .exchange(HOST, ORIGIN, issued.bootstrap.as_bytes())
            .expect("live session");
        let session = sessions
            .authorize_onboard(HOST, ORIGIN, &exchange.cookie, &exchange.csrf)
            .expect("admitted");
        let body = serde_json::to_vec(&serde_json::json!({
            "request_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "agent": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        }))
        .expect("body");
        let (reply, answer) = mpsc::channel();
        let job = Job::Retire {
            session,
            request: RetireRequest::parse(&body).expect("canonical"),
            reply,
        };
        (job, answer)
    }

    /// A retire queued while the switch was on is refused with `action_off` once it is off, and
    /// runs once it is on.
    #[test]
    fn a_queued_job_is_refused_when_its_switch_went_off() {
        let (job, answer) = retire_job();
        assert!(refuse_action_off(Switches::default(), job).is_none());
        let Ok(RetireOutcome::Refused { reason, .. }) = answer.recv().expect("an answer") else {
            panic!("a refusal");
        };
        assert_eq!(reason, ACTION_OFF);

        let (job, answer) = retire_job();
        let on = Switches {
            control: FloorControl::On {
                goal: false,
                retire: true,
                renew: false,
            },
            ..Switches::default()
        };
        assert!(refuse_action_off(on, job).is_some(), "the switch is on");
        assert!(answer.try_recv().is_err(), "nothing was answered");
    }
}
