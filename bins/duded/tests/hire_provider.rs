//! `agent.hire` refuses a provider this daemon has no adapter for, and journals nothing when it
//! does.
//!
//! # What this is a test of, and what it would be without the control
//!
//! The registry is `dudes_providers::registry::Adapters`, built once from the operator's
//! `[providers]` table. Two readers ask it: `Supervisor::launch`, when it composes a turn, and the
//! daemon's `Ask::Hire`, before it builds the record. This file is about the second — and about
//! the *pair*, because a refusal that fired on every hire would satisfy the first half of the
//! sentence above while making the daemon useless.
//!
//! The refusal is not decoration. Without it a hire on a provider with no adapter is journaled,
//! folded and shown on the floor as an agent; `launch` then finds no adapter and returns, on every
//! decide, for ever — an agent that exists, costs a row and can never take a turn, with no record
//! anywhere saying why. That is the state this file proves the daemon no longer reaches.
//!
//! # Why it asserts on the journal rather than on the reply alone
//!
//! `Record::AgentHired` cannot be un-journaled (invariant 1). So "refused" has to mean *refused
//! before the append*, not "answered with an error after the record reached the disk" — and only
//! the disk can tell those two apart. `Fixture::records` reads the same files the daemon is
//! writing.
#![cfg(unix)]

use std::sync::Arc;

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::Record;
use dudes_proto::rpc::agent::HireParams;
use dudes_proto::rpc::{ErrorCode, RpcError};
use dudes_types::{IsolationMode, Provider};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

/// How many `AgentHired` records are on the disk.
fn hired(fixture: &Fixture) -> usize {
    fixture
        .records()
        .iter()
        .filter(|record| matches!(record, Record::AgentHired { .. }))
        .count()
}

/// Ask for a hire on `provider`, through the same port `agent.hire` reaches the loop by.
fn hire_on(
    handle: &duded::Handle,
    fixture: &Fixture,
    provider: Provider,
) -> Result<duded::ports::Answered, RpcError> {
    handle.ask(duded::ports::Ask::Hire(Box::new(HireParams {
        provider,
        ..common::hire_params(fixture.state.path())
    })))
}

/// A daemon whose registry holds the shipped `[providers.claude]` adapter and nothing else — which
/// is what `common::seams` builds — refuses a hire on every other provider, naming it, and writes
/// no record for any of them.
///
/// **Five arms, and each kills a different wrong implementation.**
///
/// 1. *Every* provider but Claude is refused, not only the `Provider::Codex` alone.
///    One provider alone passes on a registry that special-cases codex.
/// 2. Each refusal **names the provider that was asked for**. A single hardcoded message
///    mentioning one provider passes arm 1 and fails here.
/// 3. No refusal carries a run of two spaces or a newline. Arms 1 and 2 are `contains`, which is
///    blind to everything but the name — measured: replacing the whole body with
///    `format!("{provider}")` left the workspace green, and so did re-planting the eaten `\`.
/// 4. One provider's message is pinned **whole**. Under `data: None` the text is the only
///    machine-readable thing a client has, so it gets a wire shape's treatment: `assert_eq!`.
/// 5. The control: the same call on `Provider::Claude` is admitted and puts exactly one
///    `AgentHired` on the disk. Without it a daemon that refused *every* hire — the trivial way to
///    make arms 1–4 green — passes this file.
///
/// The journal is read at three moments rather than one, so the count cannot be satisfied by a
/// record that arrives late: zero before, zero after nine refusals, one after the admitted hire.
#[test]
fn a_hire_on_a_provider_with_no_adapter_is_refused_by_name_and_journals_nothing() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::new();
    let daemon = boot(
        &fixture.settings(),
        // No turn is ever issued here — nothing sets a goal — so the predicate script is never
        // consulted; it is supplied because `Seams` requires a runner.
        seams(
            &settings,
            host.clone(),
            Arc::new(ScriptedRunner::scripted([common::passes()])),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    assert_eq!(hired(&fixture), 0, "nothing is hired before the test hires");

    let (mut codex_message, mut copilot_message) = (None, None);
    for provider in Provider::ALL
        .into_iter()
        .filter(|provider| *provider != Provider::Claude)
    {
        let refusal = hire_on(&running.handle, &fixture, provider)
            .expect_err("a provider with no adapter must not be hired");
        assert_eq!(
            refusal.code,
            ErrorCode::Refused.code(),
            "{provider} is refused with -32004, not some other failure: {refusal:?}"
        );
        assert!(
            refusal.message.contains(provider.as_str()),
            "the refusal must name the provider that was asked for; \
             {provider} got: {}",
            refusal.message
        );
        // The message is one line of prose with single spaces. `contains` above cannot see
        // whitespace damage, and the damage has a specific cause: a `\` line-continuation lost
        // while editing leaves a newline and the next line's indent *inside* the string.
        assert!(
            !refusal.message.contains("  ") && !refusal.message.contains('\n'),
            "the refusal is one line with no run of two spaces; {provider} got: {:?}",
            refusal.message
        );
        match provider {
            Provider::Codex => codex_message = Some(refusal.message.clone()),
            Provider::Copilot => copilot_message = Some(refusal.message.clone()),
            _ => {}
        }
    }

    // And the whole string for one provider, because `contains` pins only the name. Under
    // `data: None` this text **is** the machine-readable contract, so it is asserted the way a
    // wire shape is: exactly, byte for byte. Lifted off the daemon's own reply rather than copied
    // from the source literal — measured 153 bytes, no run of two spaces, no newline — so the
    // fixture cannot inherit a defect from the code it is checking.
    assert_eq!(
        codex_message.expect("Provider::ALL contains Codex"),
        "this daemon has no codex adapter, so a codex agent could never be given a turn; \
         the hire is refused rather than journaled as an agent that can never work; add an \
         empty [providers.codex] table to the operator configuration (dudes config set \
         providers.codex on) and restart the daemon",
        "the refusal text is the contract while `RefusedData.gate` has no value for it; \
         changing it is a wire change and this is where that is noticed"
    );
    assert_eq!(
        copilot_message.expect("Provider::ALL contains Copilot"),
        "this daemon has no copilot adapter, so a copilot agent could never be given a turn; \
         the hire is refused rather than journaled as an agent that can never work",
        "a provider with no adapter type is not sent to a [providers] table the daemon refuses"
    );
    assert_eq!(
        hired(&fixture),
        0,
        "a refused hire journals nothing: the gate stands in front of the append, and an \
         `AgentHired` cannot be un-journaled"
    );

    // The control. Everything above is green on a daemon that refuses every hire.
    let wanted = IsolationMode::Inherit;
    let answer = running
        .handle
        .ask(duded::ports::Ask::Hire(Box::new(HireParams {
            isolation: Some(wanted),
            ..common::hire_params(fixture.state.path())
        })))
        .expect("the configured provider is admitted");
    let duded::ports::Answered::Hired(result) = answer else {
        panic!("agent.hire returned the wrong answer: {answer:?}");
    };
    let agent = result.agent;
    assert_eq!(
        hired(&fixture),
        1,
        "the configured provider is still hired, and exactly once"
    );
    assert!(
        fixture.records().iter().any(|record| matches!(
            record,
            Record::AgentHired {
                id,
                provider,
                isolation,
                ..
            } if *id == agent
                && *provider == Provider::Claude
                && *isolation == Some(wanted)
        )),
        "the admitted hire is the Claude one, on the id and with the isolation selection the \
         call carried"
    );

    running.stop(ExitCause::Ended);
}
