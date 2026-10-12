//! `agent.hire` refuses an explicit `sandbox` value.
//!
//! No adapter carries a sandbox instruction: `AgentHired.sandbox` reaches `AgentCtx.sandbox` and
//! stops there, so `true` and `false` are equally inert and an operator who asks for containment
//! gets none. Until an adapter can carry it, the honest answer at the call is `-32004 Refused`.
//!
//! # Why these tests drive the handler and not a socket
//!
//! The handler *is* the only seam: `Control::hire` is reached through it and nowhere else,
//! so a refusal in front of `control.hire` is a refusal for the CLI and for a direct JSON-RPC
//! caller alike. `tests/methods.rs` already proves this handler's answer travels a real socket, and
//! that file is `#![cfg(unix)]` because of it; a refusal is platform-independent logic and is
//! asserted where all three CI targets run it.
//!
//! # What makes these tests discriminate
//!
//! A method that refused *everything* would pass a refusal-only file, so the refusals are pinned
//! against two admissions:
//!
//! - **the key absent** — the control, and the shape every caller that never sets a
//!   sandbox sends;
//! - **the key present and `null`** — the sharper one. `Option<bool>` reads `null` as `None`, which
//!   is the wire's own spelling of *"the provider's configured default"*, and an absent or `None` sandbox is admitted.
//!   An implementation that keyed on *the key being present* rather than on `Some(_)` refuses this
//!   and passes every other test in this file.
//!
//! And the refusal is asserted to have happened **before** `control.hire`: `FakeControl` records
//! what it was asked, so an empty record is what says nothing reached persist. That a refused
//! hire journals nothing, because it never reaches `persist`, is this assertion, not a comment.
//!
//! # Admitting a call is only half the rule
//!
//! The other half is that an admitted hire reaches `Control` **unmodified**. A handler that
//! inserted `request.sandbox = Some(false)` between the gate and `control.hire` would answer `ok`
//! to an operator who asked for nothing and turn their silence into an explicit *"no sandbox"*,
//! which is `AgentHired { sandbox: Some(false) }` in the journal — the false-containment record
//! the refusal exists to prevent, arrived at from the other direction.
//!
//! So `FakeControl::hire` records the `sandbox` field, and both admission tests assert
//! `sandbox=None` reached `Control`.

mod common;

use std::sync::Arc;

use common::{FakeControl, HIRED, hire_params};
use dudes_proto::rpc::agent::HireResult;
use dudes_proto::rpc::{ErrorCode, Method, RpcError};
use dudes_rpc::auth::PeerRole;
use dudes_rpc::dispatch::Call;
use dudes_rpc::methods::agent;
use dudes_rpc::outbound::Outbound;
use dudes_types::Provider;
use serde_json::{Value, json};

/// The well-formed hire of `common::hire_params`, with `sandbox` set to `value` — or left as the
/// fixture sends it, which is with no `sandbox` key at all, when `value` is `None`.
fn params(provider: Provider, sandbox: Option<Value>) -> Value {
    let mut request = json!(hire_params());
    let object = request.as_object_mut().expect("HireParams is an object");
    object.insert("provider".to_owned(), json!(provider));
    assert!(
        !object.contains_key("sandbox"),
        "the fixture's `sandbox` is `None` and skipped, so this file sets the key or omits it"
    );
    if let Some(value) = sandbox {
        object.insert("sandbox".to_owned(), value);
    }
    request
}

/// Run the `agent.hire` handler over `request`, returning its answer and what the control was
/// asked. An empty second element means `Control::hire` never ran.
fn hire(request: &Value) -> (Result<Value, RpcError>, Vec<String>) {
    let control = FakeControl::new();
    let handler = agent::hire(Arc::clone(&control));
    let role = PeerRole::Operator;
    let outbound = Arc::new(Outbound::sink());
    let answer = handler(Call {
        goal_replacement: None,
        method: Method::AgentHire.into(),
        params: Some(request),
        role: &role,
        outbound: &outbound,
        hang_up: dudes_rpc::HangUp::never(),
    });
    (answer, control.calls())
}

/// The error of a call that was expected to be refused.
fn refusal(answer: Result<Value, RpcError>) -> RpcError {
    answer.expect_err("an explicit sandbox value is refused")
}

/// `sandbox: true` — the polarity an operator reaches for when they want containment, and the one
/// that would be a false promise.
#[test]
fn an_explicit_sandbox_true_is_refused() {
    let (answer, calls) = hire(&params(Provider::Claude, Some(json!(true))));

    let refused = refusal(answer);
    assert_eq!(refused.code, ErrorCode::Refused.code(), "{refused:?}");
    assert!(
        calls.is_empty(),
        "the refusal is in front of the hire: {calls:?}"
    );
}

/// `sandbox: false` — refused too, and for a reason of its own: a trusted repository's
/// own settings can turn the vendor sandbox on, so "off" is as false a
/// promise as "on".
#[test]
fn an_explicit_sandbox_false_is_refused() {
    let (answer, calls) = hire(&params(Provider::Claude, Some(json!(false))));

    let refused = refusal(answer);
    assert_eq!(refused.code, ErrorCode::Refused.code(), "{refused:?}");
    assert!(
        calls.is_empty(),
        "the refusal is in front of the hire: {calls:?}"
    );
}

/// The control: with no `sandbox` key the hire is answered, so the two tests above
/// are about the field and not about a handler that refuses every call.
///
/// It also pins what reached [`Control`](dudes_rpc::methods::Control): `sandbox=None`. Admitting
/// the call is only half the rule — the other half is that the handler passes the operator's
/// request through untouched, and a handler that answered `ok` while writing `Some(false)` into
/// the params would journal `AgentHired { sandbox: Some(false) }` for an operator who asked for
/// nothing. That is the false-containment record the refusal exists to prevent, so the
/// assertion is on the recorded call and not only on the answer.
#[test]
fn a_hire_with_no_sandbox_key_is_answered_with_none_reaching_control() {
    let (answer, calls) = hire(&params(Provider::Claude, None));

    let value = answer.expect("an absent sandbox is admitted");
    let hired: HireResult = serde_json::from_value(value).expect("a HireResult");
    assert_eq!(hired.agent.to_string(), HIRED, "the daemon's own answer");
    assert_eq!(
        calls,
        vec!["hire Kim sandbox=None".to_owned()],
        "the params reached Control unmodified"
    );
}

/// The sharper control: `sandbox: null` is `None`, which is *"the provider's configured default"*
/// and carries no instruction, so it is admitted like an absent key.
///
/// This is the case that separates a refusal keyed on `Some(_)` from one keyed on the key being
/// present. `HireParams.sandbox` is `Option<bool>` and `null` decodes to `None`, so a client that
/// serialises the whole struct rather than skipping its `None`s is a client that says nothing —
/// and refusing it would refuse a caller who asked for nothing.
///
/// `sandbox=None` is asserted here for the same reason as in the test above, and for one more:
/// `null` is the case where a handler could plausibly "normalise" the field on the way past. It
/// must arrive as `None`, indistinguishable from an absent key, because that is what the journal
/// will hold.
#[test]
fn an_explicit_null_sandbox_is_answered_with_none_reaching_control() {
    let (answer, calls) = hire(&params(Provider::Claude, Some(Value::Null)));

    let value = answer.expect("a null sandbox is `None`, and `None` is admitted");
    let hired: HireResult = serde_json::from_value(value).expect("a HireResult");
    assert_eq!(hired.agent.to_string(), HIRED);
    assert_eq!(
        calls,
        vec!["hire Kim sandbox=None".to_owned()],
        "`null` reaches Control as `None`, exactly as an absent key does"
    );
}

/// The words of `message`, split on everything that is not alphanumeric.
///
/// **Substring containment is unsound over this provider set**, which is why the assertions below
/// go through here. `Provider::Copilot` is `"copilot"` and `Provider::Pi` is `"pi"`
/// (`crates/dudes-types/src/provider.rs`), so `"copilot".contains("pi")` is true: asserting that a
/// *copilot* refusal does not mention Pi fails on a message that is perfectly correct. Comparing
/// whole words instead makes the check say what it means.
///
/// Splitting on non-alphanumerics rather than on whitespace matters too — the message ends
/// `…to get claude's configured default`, and a whitespace split yields `claude's`, so a second
/// occurrence of the provider's own name would be invisible to a word check.
fn words(message: &str) -> Vec<&str> {
    message
        .split(|c: char| !c.is_ascii_alphanumeric())
        .collect()
}

/// The message is the whole story the operator gets, so it names the provider it was asked for —
/// read from the params, not written into the handler.
///
/// Each provider is asserted to name itself and *not* the other, both as whole words: a hard-coded
/// `claude` passes a one-provider check, and the `copilot`/`pi` rows are the pair a substring
/// assertion gets wrong in the *passing* direction.
#[test]
fn the_refusal_names_the_provider_it_was_asked_for() {
    for (provider, other) in [
        (Provider::Claude, Provider::Codex),
        (Provider::Codex, Provider::Claude),
        // The failing case for `words`: under substring containment this row is red, because
        // `"copilot".contains("pi")`.
        (Provider::Copilot, Provider::Pi),
        (Provider::Pi, Provider::Copilot),
    ] {
        let (answer, _) = hire(&params(provider, Some(json!(true))));

        let message = refusal(answer).message;
        assert!(
            words(&message).contains(&provider.as_str()),
            "{provider} is missing from: {message}"
        );
        assert!(
            !words(&message).contains(&other.as_str()),
            "{other} is not what was asked for: {message}"
        );
    }
}
