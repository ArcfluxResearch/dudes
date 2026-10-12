//! `agent.hire`, `agent.rename` and `agent.look` refuse a name or role holding a control character
//! before `Control` sees it, and admit bidirectional formatting characters, which are not controls.

mod common;

use std::sync::Arc;

use common::{FakeControl, HIRED, hire_params};
use dudes_proto::rpc::{ErrorCode, Method, RpcError};
use dudes_rpc::auth::PeerRole;
use dudes_rpc::dispatch::Call;
use dudes_rpc::methods::agent;
use dudes_rpc::outbound::Outbound;
use serde_json::{Value, json};

fn call(
    method: Method,
    handler: impl Fn(Call<'_>) -> Result<Value, RpcError>,
    request: &Value,
) -> Result<Value, RpcError> {
    let role = PeerRole::Operator;
    let outbound = Arc::new(Outbound::sink());
    handler(Call {
        goal_replacement: None,
        method: method.into(),
        params: Some(request),
        role: &role,
        outbound: &outbound,
        hang_up: dudes_rpc::HangUp::never(),
    })
}

fn hire(name: &str, role: &str) -> (Result<Value, RpcError>, Vec<String>) {
    let mut request = json!(hire_params());
    request["name"] = json!(name);
    request["role"] = json!(role);
    let control = FakeControl::new();
    let answer = call(
        Method::AgentHire,
        agent::hire(Arc::clone(&control)),
        &request,
    );
    (answer, control.calls())
}

#[test]
fn hire_refuses_a_control_character_in_the_name_or_the_role() {
    for (name, role) in [
        ("Kim\u{1b}]52;c;aGk=\u{7}", "backend engineer"),
        ("Kim", "backend\u{1b}[2J engineer"),
        ("Kim", "backend\nengineer"),
        ("Kim", "backend\u{9b}1m engineer"),
    ] {
        let (answer, calls) = hire(name, role);
        let refused = answer.expect_err("a control character is refused");
        assert_eq!(refused.code, ErrorCode::InvalidParams.code(), "{refused:?}");
        assert!(calls.is_empty(), "refused before the hire: {calls:?}");
    }

    let (answer, calls) = hire("Kim", &format!("{}\n", "x".repeat(513)));
    let refused = answer.expect_err("the existing control refusal takes precedence");
    assert_eq!(refused.message, "role must hold no control character");
    assert!(calls.is_empty());

    let (answer, calls) = hire("Kim\u{202e}", "backend\u{2066} engineer");
    assert!(answer.is_ok(), "{answer:?}");
    assert_eq!(calls, ["hire Kim\u{202e} sandbox=None"]);
}

#[test]
fn hire_refuses_a_role_over_512_utf8_bytes_before_the_control() {
    for role in [
        "x".repeat(513),
        format!("{}x", "é".repeat(256)),
        " ".repeat(513),
    ] {
        assert_eq!(role.len(), 513);
        let (answer, calls) = hire("Kim", &role);
        let refused = answer.expect_err("a 513-byte role is refused");
        assert_eq!(refused.code, ErrorCode::InvalidParams.code());
        assert_eq!(refused.message, "role must contain at most 512 UTF-8 bytes");
        assert!(calls.is_empty(), "refused before the hire: {calls:?}");
    }
}

#[test]
fn hire_admits_a_role_at_512_utf8_bytes() {
    for role in ["x".repeat(512), "é".repeat(256)] {
        assert_eq!(role.len(), 512);
        let (answer, calls) = hire("Kim", &role);
        assert!(answer.is_ok(), "{answer:?}");
        assert_eq!(calls, ["hire Kim sandbox=None"]);
    }
}

/// `FakeControl` leaves `rename` and `look` to the trait's defaults, which answer `InternalError`,
/// so that code is the proof a clean name or role reached it.
#[test]
fn rename_and_look_refuse_a_control_character_before_the_control() {
    let rename = |name: &str| {
        call(
            Method::AgentRename,
            agent::rename(FakeControl::new()),
            &json!({ "agent": HIRED, "name": name }),
        )
        .expect_err("the fake control implements no rename")
        .code
    };
    assert_eq!(rename("Kim\u{1b}[2J"), ErrorCode::InvalidParams.code());
    assert_eq!(rename("Kim\u{202e}"), ErrorCode::InternalError.code());

    let look = |role: &str| {
        call(
            Method::AgentLook,
            agent::look(FakeControl::new()),
            &json!({ "agent": HIRED, "role": role, "character": null, "accent": null }),
        )
        .expect_err("the fake control implements no look")
        .code
    };
    assert_eq!(
        look("engi\u{1b}]52;c;aGk=\u{7}neer"),
        ErrorCode::InvalidParams.code()
    );
    assert_eq!(look("engi\nneer"), ErrorCode::InvalidParams.code());
    assert_eq!(look("engi\u{2066}neer"), ErrorCode::InternalError.code());
}
