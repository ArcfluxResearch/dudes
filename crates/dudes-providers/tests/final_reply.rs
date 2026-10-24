//! Completed replies retain conversation text independently of the legacy outcome summary.

use dudes_proto::{ExitStatus, TurnOutcome};
use dudes_providers::stream::{FinalReplyCompleteness, TurnEnd, TurnReader};
use dudes_providers::{claude, codex, pi};
use serde_json::{Value, json};

fn finish(mut reader: Box<dyn TurnReader>, events: Vec<Value>, code: i32) -> TurnEnd {
    for event in events {
        reader.line(&event.to_string());
    }
    reader.end(ExitStatus {
        code: Some(code),
        signal: None,
    })
}

fn replies(text: &str, code: i32) -> Vec<TurnEnd> {
    vec![
        finish(
            Box::new(claude::stream::TurnStream::new()),
            vec![
                json!({"type":"system","subtype":"init","session_id":"session"}),
                json!({"type":"result","subtype":"success","session_id":"session","result":text}),
            ],
            code,
        ),
        finish(
            Box::new(codex::stream::TurnStream::new()),
            vec![
                json!({"type":"thread.started","thread_id":"01a0e1d9-3860-7800-a9ff-2994ef3b1c6f"}),
                json!({"type":"item.completed","item":{"type":"agent_message","text":text}}),
                json!({"type":"turn.completed"}),
            ],
            code,
        ),
        finish(
            Box::new(pi::stream::TurnStream::new()),
            vec![
                json!({"type":"session","id":"session"}),
                json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":text}],"stopReason":"stop"}}),
                json!({"type":"agent_settled"}),
            ],
            code,
        ),
    ]
}

#[test]
fn completed_replies_preserve_text_beyond_the_legacy_limit() {
    for length in [4096, 5000, 65_536] {
        let text = "x".repeat(length);
        for end in replies(&text, 0) {
            assert_eq!(end.outcome, TurnOutcome::Completed);
            let reply = end.final_reply.unwrap();
            assert_eq!(reply.text, text);
            assert_eq!(reply.completeness, FinalReplyCompleteness::Complete);
            assert_eq!(end.last_message.unwrap(), text);
        }
    }
}

#[test]
fn oversized_completed_replies_leave_whole_and_cut_on_a_utf8_boundary_when_bounded() {
    let text = "é".repeat(32_769);
    let expected = format!("{}…", "é".repeat(32_766));
    for end in replies(&text, 0) {
        let whole = end.final_reply.unwrap();
        assert_eq!(whole.text, text);
        assert_eq!(whole.completeness, FinalReplyCompleteness::Complete);
        let reply = whole.bounded();
        assert_eq!(reply.text, expected);
        assert_eq!(reply.completeness, FinalReplyCompleteness::Truncated);
        assert_eq!(
            dudes_providers::stream::bound(&end.last_message.unwrap()).len(),
            4095
        );
    }
}

#[test]
fn failed_turns_carry_no_completed_reply() {
    for end in replies("only provisional", 1) {
        assert!(matches!(end.outcome, TurnOutcome::Failed { .. }));
        assert_eq!(end.final_reply, None);
        assert_eq!(end.last_message, None);
    }
    for end in replies("completed", 0) {
        assert_eq!(end.final_reply.unwrap().text, "completed");
    }
}

#[test]
fn an_unattributed_claude_result_cannot_supply_a_completed_reply() {
    let end = finish(
        Box::new(claude::stream::TurnStream::new()),
        vec![
            json!({"type":"system","subtype":"init","session_id":"session"}),
            json!({"type":"result","subtype":"success","session_id":"other","result":"foreign"}),
        ],
        0,
    );
    assert_eq!(end.final_reply, None);
    assert_eq!(end.outcome, TurnOutcome::NoResult);
    assert_eq!(
        replies("control", 0)[0].final_reply.as_ref().unwrap().text,
        "control"
    );
}
