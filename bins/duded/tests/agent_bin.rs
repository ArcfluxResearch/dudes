//! The daemon's one-entry `agent-bin` directory across a run: built once the runtime directory
//! exists, pointing at the `dudes` beside the daemon's executable, and gone after a clean stop.
#![allow(missing_docs)]
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use duded::startup::Witness;
use duded::{ExitCause, boot};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock};

#[test]
fn agent_bin_holds_one_link_while_running_and_is_gone_after_a_clean_stop() {
    let fixture = Fixture::new();
    let bin = fixture.state.path().join("install-bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(bin.join("dudes"), b"").unwrap();
    let mut settings = fixture.settings();
    settings.daemon_exe = Some(bin.join("duded"));
    let agent_bin = settings.runtime_dir.join("agent-bin");

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
    .unwrap();
    let mut running = common::start(daemon);

    let entries: Vec<_> = std::fs::read_dir(&agent_bin)
        .expect("the directory exists while the daemon runs")
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, [std::ffi::OsString::from("dudes")]);
    assert_eq!(
        std::fs::read_link(agent_bin.join("dudes")).unwrap(),
        bin.join("dudes")
    );
    assert_eq!(
        std::fs::metadata(&agent_bin).unwrap().permissions().mode() & 0o777,
        0o700
    );

    running.stop(ExitCause::Ended);
    assert!(!agent_bin.exists(), "a clean stop removes the directory");
}
