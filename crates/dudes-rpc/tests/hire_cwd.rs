//! `agent.hire` refuses a relative `cwd` before `Control` sees it, so no `AgentHired` records a
//! directory the daemon would resolve against its own working directory.

mod common;

use std::sync::Arc;

use common::{FakeControl, hire_params};
use dudes_proto::rpc::{ErrorCode, Method, RpcError};
use dudes_rpc::auth::PeerRole;
use dudes_rpc::dispatch::Call;
use dudes_rpc::methods::agent;
use dudes_rpc::outbound::Outbound;
use serde_json::{Value, json};

fn hire(cwd: &str) -> (Result<Value, RpcError>, Vec<String>) {
    let mut request = json!(hire_params());
    request["cwd"] = json!(cwd);
    let control = FakeControl::new();
    let role = PeerRole::Operator;
    let outbound = Arc::new(Outbound::sink());
    let answer = agent::hire(Arc::clone(&control))(Call {
        goal_replacement: None,
        method: Method::AgentHire.into(),
        params: Some(&request),
        role: &role,
        outbound: &outbound,
        hang_up: dudes_rpc::HangUp::never(),
    });
    (answer, control.calls())
}

#[test]
fn hire_refuses_a_relative_cwd() {
    for (cwd, message) in [
        (".", "cwd `.` is not an absolute path"),
        ("repo", "cwd `repo` is not an absolute path"),
        ("../repo", "cwd `../repo` is not an absolute path"),
        ("", "cwd `` is not an absolute path"),
    ] {
        let (answer, calls) = hire(cwd);
        let refused = answer.expect_err("a relative cwd is refused");
        assert_eq!(refused.code, ErrorCode::InvalidParams.code(), "{refused:?}");
        assert_eq!(refused.message, message);
        assert!(calls.is_empty(), "refused before the hire: {calls:?}");
    }

    let absolute = std::env::temp_dir().join("dudes-fixture");
    let (answer, calls) = hire(absolute.to_str().expect("a UTF-8 temporary directory"));
    assert!(answer.is_ok(), "{answer:?}");
    assert_eq!(calls, ["hire Kim sandbox=None"]);
}
