//! Strict MCP edit decoding and one-pass propagation of the writer's answer.

mod common;

use std::sync::Arc;

use common::{FakeControl, HIRED, TARGET};
use dudes_proto::rpc::{ErrorCode, Method};
use dudes_rpc::auth::PeerRole;
use dudes_rpc::dispatch::Call;
use dudes_rpc::methods::agent;
use dudes_rpc::outbound::Outbound;
use serde_json::{Value, json};

fn invoke(control: Arc<FakeControl>, request: &Value) -> Result<Value, dudes_proto::rpc::RpcError> {
    let outbound = Arc::new(Outbound::sink());
    agent::mcp(control)(Call {
        goal_replacement: None,
        method: Method::AgentMcp.into(),
        params: Some(request),
        role: &PeerRole::Operator,
        outbound: &outbound,
        hang_up: dudes_rpc::HangUp::never(),
    })
}

#[test]
fn read_clear_and_sorted_replacement_reach_control_without_changing_its_result() {
    for (request, recorded) in [
        (json!({"agent": TARGET}), json!({"agent": TARGET})),
        (
            json!({"agent": TARGET, "mcp_servers": []}),
            json!({"agent": TARGET, "mcp_servers": []}),
        ),
        (
            json!({"agent": TARGET, "mcp_servers": ["search", "memory"]}),
            json!({"agent": TARGET, "mcp_servers": ["memory", "search"]}),
        ),
    ] {
        let control = FakeControl::new();
        assert_eq!(
            invoke(Arc::clone(&control), &request).unwrap(),
            json!({
                "agent": HIRED, "mcp_servers": ["search"], "changed": false
            })
        );
        assert_eq!(
            control.calls(),
            [format!("mcp {}", serde_json::to_string(&recorded).unwrap())]
        );
    }
}

#[test]
fn invalid_replacement_never_reaches_control() {
    for request in [
        json!({"agent": TARGET, "mcp_servers": null}),
        json!({"agent": TARGET, "mcp_servers": ["memory", "memory"]}),
        json!({"agent": TARGET, "mcp_servers": ["Memory"]}),
        json!({"agent": TARGET, "trusted": true}),
        json!({"mcp_servers": []}),
    ] {
        let control = FakeControl::new();
        assert_eq!(
            invoke(Arc::clone(&control), &request).unwrap_err().code,
            ErrorCode::InvalidParams.code()
        );
        assert!(control.calls().is_empty());
    }
}

#[test]
fn writer_refusals_reach_the_caller_without_retry() {
    for code in [
        ErrorCode::NotFound,
        ErrorCode::Refused,
        ErrorCode::InternalError,
    ] {
        let control = FakeControl::refusing(code);
        let refused = invoke(
            Arc::clone(&control),
            &json!({"agent": TARGET, "mcp_servers": []}),
        )
        .unwrap_err();
        assert_eq!(refused.code, code.code());
        assert_eq!(refused.message, "the fake control refused");
        assert_eq!(control.calls().len(), 1);
    }
}
