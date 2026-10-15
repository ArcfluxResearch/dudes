//! Unproved Linux capture recipes fall back before admission or child creation.
#![cfg(target_os = "linux")]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Fixture, ScriptedRunner, SharedHost, TestClock};
use duded::ports::{Answered, Ask};
use duded::predicate::PredicateOutcome;
use duded::startup::Witness;
use dudes_journal::MemoryCondenseConfig;
use dudes_proto::{PeerProvenance, Record};

#[test]
fn unproved_linux_capture_uses_exact_excerpts_before_admission() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.condense_model_enabled = true;
    let runner = Arc::new(ScriptedRunner::scripted([PredicateOutcome::Finished {
        exit: Some(0),
        output_tail: serde_json::json!({"type":"result","subtype":"success","is_error":false,
            "result":"{\"condensed\":\"an unsupported call must not run\"}"})
        .to_string(),
        timed_out: false,
    }]));
    let daemon = duded::boot::boot_with_memory_config(
        &settings,
        common::seams(
            &settings,
            SharedHost::new(),
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
        MemoryCondenseConfig::new(1, 2).unwrap(),
    )
    .unwrap();
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    for _ in 0..3 {
        assert!(matches!(
            running
                .handle
                .ask(Ask::MemoryAppend {
                    agent,
                    payload: vec![b'x'; 2048],
                    provenance: PeerProvenance::NoRecordedAgent,
                })
                .unwrap(),
            Answered::MemoryAppended(_)
        ));
    }
    let bound = Instant::now() + Duration::from_secs(10);
    let summary = loop {
        if let Some(path) = fixture.records().iter().find_map(|record| match record {
            Record::MemoryCondense { summary_ref, .. } => Some(summary_ref.clone()),
            _ => None,
        }) {
            break path;
        }
        assert!(Instant::now() < bound);
        std::thread::sleep(Duration::from_millis(10));
    };
    running.stop(duded::ExitCause::Ended);
    assert!(
        std::fs::read_to_string(fixture.state.path().join(summary))
            .unwrap()
            .contains("\"kind\":\"exact\"")
    );
    assert_eq!(runner.runs(), 0);
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::MemoryCondenseModelAdmitted { .. }))
    );
}
