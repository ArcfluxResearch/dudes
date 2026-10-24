//! A turn's step counts and context size are display evidence. Two runs that differ only in what
//! the reader reported must journal the same decisions, gate traces and breaker beats.

use dudes_proto::goal::Evidence;
use dudes_proto::{ContextReport, ExitStatus, Record, StepDigest, TurnReport};
use dudes_supervisor::Input;
use dudes_supervisor::testing::Harness;

fn run(report: impl Fn() -> TurnReport) -> Vec<Record> {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    for _ in 0..4 {
        let turn = harness.open_turn(agent).expect("the loop issued a turn");
        harness.drive(Input::ChildExited {
            report: report(),
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: None,
            model_usage: std::collections::BTreeMap::new(),
            outcome: dudes_proto::TurnOutcome::Completed,
            last_message: None,
        });
        harness.drive(Input::PredicateDone {
            goal,
            pass: false,
            evidence: Evidence::Command {
                argv: vec!["npm".into(), "test".into()],
                program: Some("/opt/toolchain/bin/npm".into()),
                exit: Some(1),
                output_tail: String::new(),
            },
        });
    }
    harness.records().cloned().collect()
}

fn without_report(records: &[Record]) -> Vec<Record> {
    records
        .iter()
        .cloned()
        .map(|mut record| {
            if let Record::TurnEnded {
                step_digest,
                context,
                ..
            } = &mut record
            {
                *step_digest = None;
                *context = None;
            }
            record
        })
        .collect()
}

#[test]
fn steps_and_context_never_change_a_decision() {
    let heavy = || TurnReport {
        step_digest: Some(StepDigest {
            command: u32::MAX,
            edit: u32::MAX,
            files_edited: u32::MAX,
            ..StepDigest::default()
        }),
        context: Some(ContextReport {
            model: "claude-sonnet-4-5".into(),
            window: 200_000,
            used: 199_999,
        }),
    };
    let plain = run(TurnReport::default);
    let reported = run(heavy);
    assert!(
        plain
            .iter()
            .any(|record| matches!(record, Record::LoopDecided { .. }))
    );
    assert!(reported.iter().any(|record| matches!(
        record,
        Record::TurnEnded {
            step_digest: Some(_),
            context: Some(_),
            ..
        }
    )));
    assert_eq!(without_report(&plain), without_report(&reported));
}
