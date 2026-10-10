//! A ticket's start installs a goal, so the daemon's goal-keyed seams meet it. It reads the new
//! goal's spend like a browser goal change does, and it is no browser action: no fault point,
//! pause point or session attribution fires for it.

use std::sync::mpsc;
use std::time::Duration;

use dudes_proto::record::samples;
use dudes_proto::{Record, RecordKind};
use dudes_types::{AgentId, GoalId};

use super::{
    AppendStage, OnboardingFailure, OnboardingFailurePoint, OnboardingPause, goal_to_price,
};
use super::{OnboardingPausePoint, browser_chat};

fn ticket_start() -> Record {
    samples::record(RecordKind::TicketStarted)
}

fn plain_goal_set() -> Record {
    samples::record(RecordKind::GoalSet)
}

fn identity(record: &Record) -> (AgentId, GoalId) {
    let (agent, goal) = record.goal_install().expect("a goal carrier");
    (agent, goal.id)
}

#[test]
fn spend_refreshes_after_a_ticket_start() {
    let start = ticket_start();
    assert_eq!(goal_to_price(&start), Some(identity(&start)));
}

/// The control: a plain goal set starts from nothing and prices nothing, so the ticket's inclusion
/// is a decision and not a side effect of matching every goal carrier.
#[test]
fn a_plain_goal_set_prices_nothing() {
    assert_eq!(goal_to_price(&plain_goal_set()), None);
}

const FAULT_POINTS: [OnboardingFailurePoint; 15] = [
    OnboardingFailurePoint::Hire,
    OnboardingFailurePoint::HireProjection,
    OnboardingFailurePoint::Goal,
    OnboardingFailurePoint::LocalGoal,
    OnboardingFailurePoint::LocalRetire,
    OnboardingFailurePoint::TurnIssued,
    OnboardingFailurePoint::ClosingAnnounced,
    OnboardingFailurePoint::ClosingAck,
    OnboardingFailurePoint::ClosingComplete,
    OnboardingFailurePoint::Answer,
    OnboardingFailurePoint::Control,
    OnboardingFailurePoint::GoalChange,
    OnboardingFailurePoint::Retire,
    OnboardingFailurePoint::Rename,
    OnboardingFailurePoint::Look,
];

#[test]
fn ticket_start_hits_no_browser_fault_point() {
    let failure = OnboardingFailure::default();
    for point in FAULT_POINTS {
        failure.arm(point);
        assert!(!failure.take(&ticket_start()), "{point:?}");
    }
    failure.arm(OnboardingFailurePoint::LocalGoal);
    assert!(
        failure.take(&plain_goal_set()),
        "the control: a local goal set is a fault point"
    );
}

#[test]
fn ticket_start_hits_no_browser_pause_point() {
    use OnboardingPausePoint as Point;
    for point in [
        Point::BeforeGoalPermit,
        Point::AfterGoalPermit,
        Point::AfterGoalFsync,
        Point::BeforeGoalChangePermit,
        Point::AfterGoalChangePermit,
    ] {
        for stage in [
            AppendStage::BeforePermit,
            AppendStage::AfterPermit,
            AppendStage::AfterFsync,
        ] {
            let pause = OnboardingPause::default();
            pause.arm(point);
            let (done, finished) = mpsc::channel();
            let at = pause.clone();
            let handle = std::thread::spawn(move || {
                at.at(&ticket_start(), stage);
                let _ = done.send(());
            });
            let returned = finished.recv_timeout(Duration::from_secs(1)).is_ok();
            pause.release();
            handle.join().unwrap();
            assert!(returned, "{point:?} paused a ticket start");
            assert!(!pause.wait_reached(Duration::from_millis(1)), "{point:?}");
        }
    }
}

#[test]
fn ticket_start_has_no_browser_session() {
    assert_eq!(browser_chat::attribution_session(&ticket_start()), None);
}
