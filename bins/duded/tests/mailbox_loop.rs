//! Queued jobs do not start a mailbox pass for every job.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, hire};
use duded::startup::Witness;
use duded::{ExitCause, boot};

mod common;

#[test]
fn back_to_back_jobs_do_not_each_run_a_mailbox_pass() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        common::seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot daemon");
    let passes = daemon.mailbox_passes();
    let mut running = common::start(daemon);
    hire(&running.handle, fixture.state.path());
    let before = passes.load(Ordering::Relaxed);
    let started = Instant::now();
    for _ in 0..200 {
        running.handle.doctor().unwrap();
    }
    let ran = passes.load(Ordering::Relaxed) - before;
    let gaps = u64::try_from(started.elapsed().as_millis() / 100).unwrap();
    assert!(ran <= gaps + 2, "{ran} passes in {:?}", started.elapsed());
    running.stop(ExitCause::ShutdownRpc);
}
