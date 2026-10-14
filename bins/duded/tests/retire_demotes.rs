//! Retirement demotes the next call over a connection that is already open.
//!
//! # What the subject actually is
//!
//! **`row()` stays per call.** Only `resolve` moved into the connection frame;
//! the classifier still asks the token projection on every call, and that projection stops naming
//! a retired agent. So agent-token revocation is a property of *when the row is computed*, and an
//! implementer who read "resolve once per connection" as "compute the `PeerRole` once per
//! connection" would make a same-shaped edit, keep the suite green, and silently break it.
//!
//! Both arms below therefore make their calls **over one connection** and assert that a later call
//! is refused differently from an earlier one. A test that reconnected between them would pass
//! under that mutation and would be measuring nothing.
//!
//! # The ordering barrier is the observation, and an `Ask` would have hidden a real defect
//!
//! `Method::AgentRetire` now has an RPC route, but this publication test deliberately drives
//! `Handle::input`, which returns immediately with no reply channel to order against. The obvious
//! barrier is therefore a *subsequent* `Ask`, whose answer proves the retirement's `pump`
//! completed.
//!
//! **That barrier makes this file green under the mutation it exists to catch, and it was
//! measured rather than reasoned about.** Every `Ask` runs its own `pump`, and every `pump`
//! republishes — so a publication narrowed to fold-affecting inputs and skipping `Command::Retire`
//! is laundered by the barrier's own publication, and both arms passed. The barrier here is
//! therefore the **observation**: the same call is repeated over the same connection until it is
//! refused `-32004`, with no `Ask` between the retirement and the answer, so what the poll observes
//! can only be the retirement's own `pump` having published. Nothing else pumps — the fixture's
//! beat is an hour, so no reaper tick intervenes.
//!
//! The journal control comes first: `AgentRetired` on the disk separates "the publication did not
//! happen" from "the retirement did not happen", which a timeout alone cannot.
//!
//! # Three arms, and only one of them is evidence about the shipped daemon
//!
//! - [`retire_demotes_a_live_connection_under_the_production_pair`] runs on the daemon's own
//!   resolver and its own token projection, and moves the wire code from `-32001` to `-32004`.
//!   Nothing but `MintedTokens::owner_of` leaving row 2 can move that code, which is what makes
//!   it a complete check of the subject: an implementer who computed the role once per connection
//!   returns `-32001` for ever and fails here.
//! - [`retire_demotes_a_live_connection_through_an_injected_column`] is the original check —
//!   a connection that authorises as `Agent(A)` and is refused `-32004` after the retirement of an
//!   agent with no recorded child. It is reached through a fixed `Seams.resolver` column so the
//!   test remains independent of the calling test process's real ancestry.
//! - [`a_retired_agent_with_a_recorded_child_keeps_its_column_until_that_child_ends`] is the
//!   retired-agent check over the same fixed column: the retired agent's turn child still runs, so the connection
//!   keeps resolving to it, and the first call after that child's `TurnEnded` is refused `-32004`.
//!
//! **A green second or third arm is evidence about the fixed column, not production ancestry.**
//! The real host-backed relation is exercised in `unix_peer_provenance.rs`; these arms isolate
//! per-call row recomputation after retirement.
#![cfg(unix)]

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::ports::{Answered, Ask};
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::Record;
use dudes_proto::rpc::agent::RetireParams;
use dudes_proto::rpc::hook::HookToken;
use dudes_proto::rpc::{ErrorCode, Method, RefusalGate, RefusedData, Response};
use dudes_rpc::Client;
use dudes_rpc::auth::PeerProvenance;
use dudes_supervisor::{Command, Input};
use serde_json::json;

mod common;

use common::{Fixture, ScriptedColumn, ScriptedRunner, SharedHost, TestClock};

/// The client this file drives, spelled once.
type Connection = Client<BufReader<UnixStream>, UnixStream>;

/// The error code of a response that carries one.
fn code(response: &Response) -> i32 {
    response
        .error
        .as_ref()
        .unwrap_or_else(|| panic!("an error response, got {response:?}"))
        .code
}

/// Hire an agent, give it a goal so a turn is issued and a token is minted, and hand back the
/// agent and the plaintext of the token the journal now carries a hash of.
fn an_agent_holding_a_live_token(
    fixture: &Fixture,
    running: &common::Running,
) -> (dudes_types::AgentId, HookToken) {
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let hash = fixture.await_record("HookTokenMinted", |record| match record {
        Record::HookTokenMinted {
            agent: owner,
            token_hash,
        } if *owner == agent => Some(token_hash.clone()),
        _ => None,
    });
    fixture.await_record("the retirement subject's actual spawn", |record| {
        matches!(record, Record::TurnSpawned { agent: owner, .. } if *owner == agent).then_some(())
    });
    running
        .handle
        .doctor()
        .expect("finish the retirement subject's setup pump");
    (agent, common::token_of(&hash))
}

/// Retire `agent`, wait for the record to reach the disk, then call over `client` until the answer
/// is `-32004` — and assert the typed gate on the answer that changed.
///
/// **No `Ask` runs between the retirement and the answer**, deliberately: see this file's header.
/// The journal wait is the control that a timeout below is about the publication and not about a
/// retirement that never happened.
fn retire_and_await_demotion(
    fixture: &Fixture,
    running: &common::Running,
    agent: dudes_types::AgentId,
    client: &mut Connection,
    token: &HookToken,
) {
    running.handle.input(Input::Command(Command::Retire {
        agent,
        reason: "the quarter ended".to_owned(),
        browser_retire: None,
    }));
    fixture.await_record("AgentRetired", |record| match record {
        Record::AgentRetired { id, .. } if *id == agent => Some(()),
        _ => None,
    });
    await_demotion(client, token);
}

fn await_demotion(client: &mut Connection, token: &HookToken) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let answer = client
            .call(Method::SystemPing, Some(json!({ "token": token.expose() })))
            .expect("the connection is still answering");
        if answer.error.as_ref().map(|error| error.code) == Some(ErrorCode::Refused.code()) {
            let data: RefusedData =
                serde_json::from_value(answer.error.expect("checked").data.expect("typed data"))
                    .expect("a RefusedData");
            assert_eq!(
                data.gate,
                RefusalGate::Token,
                "row 3's refusal names the token gate, not some other refusal wearing -32004"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the retirement is on the disk and the connection is still not demoted after 10s; \
             last answer was {answer:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn on_an_injected_column(
    fixture: &Fixture,
    runner: ScriptedRunner,
) -> (
    Arc<ScriptedColumn>,
    common::Running,
    dudes_types::AgentId,
    HookToken,
    SharedHost,
) {
    let column = ScriptedColumn::new(PeerProvenance::Unresolvable);
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let seams = common::seams_with(
        host.clone(),
        Arc::new(runner),
        Arc::new(TestClock::new()),
        Witness::default(),
        Arc::clone(&column) as Arc<dyn dudes_rpc::auth::PeerResolver>,
    );
    host.bind_child_ends(Arc::clone(&seams.children));
    let daemon = boot(&fixture.settings(), seams).expect("boot");
    let running = common::start(daemon);
    let (agent, token) = an_agent_holding_a_live_token(fixture, &running);
    column.set(PeerProvenance::Agent(agent));
    (column, running, agent, token, host)
}

/// **Arm A, and the gate: the production pair alone.**
///
/// The classifier, the credential, the token projection and the resolver are all the daemon's own
/// — nothing is injected. Presenting a live agent token from a peer whose provenance the daemon
/// cannot resolve is the (known token × unresolved provenance) case, which refuses `-32001`; after the
/// retirement the same token on the same connection is an unknown token, which refuses `-32004` with the
/// typed gate.
///
/// **Only `owner_of` leaving row 2 can move that code**, so the difference between the two calls
/// is the projection being re-read.
#[test]
fn retire_demotes_a_live_connection_under_the_production_pair() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let seams = common::seams(
        host.clone(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&seams.children));
    let daemon = boot(&settings, seams).expect("boot");
    let mut running = common::start(daemon);
    let (agent, token) = an_agent_holding_a_live_token(&fixture, &running);

    let mut client = Client::connect(settings.socket()).expect("connect to the daemon");
    let before = client
        .call(Method::SystemPing, Some(json!({ "token": token.expose() })))
        .expect("answered");
    assert_eq!(
        code(&before),
        ErrorCode::Unauthorized.code(),
        "row 2 in the third column: a live token, from a peer this daemon cannot place"
    );
    assert_ne!(
        ErrorCode::Unauthorized.code(),
        ErrorCode::Refused.code(),
        "positive control: the two codes are different numbers, so the change below is a change"
    );

    retire_and_await_demotion(&fixture, &running, agent, &mut client, &token);
    common::end_turn_with_native(&fixture, &running.handle, &host, fixture.await_turn());
    running.stop(ExitCause::Ended);
}

/// **Arm B: an injected `Agent(A)` column, reachable only through `Seams.resolver`.**
///
/// A resolver answering `Agent(A)` puts the connection in row 2 column 1, which resolves to
/// `PeerRole::Agent` and is admitted by `system.ping`'s `Requirement::AnyRole`. The agent's turn
/// ends and its goal passes before the retirement, so it has no recorded child. After the
/// retirement the same connection is refused `-32004`, and the injected column is irrelevant to
/// that verdict because row 3 refuses in every column.
///
/// **This is not evidence about the production ancestry walk**, for the reason in this file's
/// header: the fixture supplies the column directly.
///
/// The `asked()` assertion is the second half of the subject: one resolution for the whole
/// connection, so the demotion cannot be a re-resolution wearing a different name.
#[test]
fn retire_demotes_a_live_connection_through_an_injected_column() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let (column, mut running, agent, token, host) =
        on_an_injected_column(&fixture, ScriptedRunner::scripted([common::passes()]));
    common::end_turn_with_native(&fixture, &running.handle, &host, fixture.await_turn());
    fixture.await_record("PredicateResult", |record| {
        matches!(record, Record::PredicateResult { pass: true, .. }).then_some(())
    });

    let mut client = Client::connect(settings.socket()).expect("connect to the daemon");
    let before = client
        .call(Method::SystemPing, Some(json!({ "token": token.expose() })))
        .expect("answered");
    assert!(
        before.error.is_none(),
        "row 2 column 1 resolves to `Agent`, and `system.ping` admits every role: {:?}",
        before.error
    );

    retire_and_await_demotion(&fixture, &running, agent, &mut client, &token);

    assert_eq!(
        column.asked(),
        1,
        "resolved once for the connection: the demotion is `row()` being re-evaluated, not the \
         column being resolved again"
    );
    running.stop(ExitCause::Ended);
}

/// **Arm C: a retired agent's recorded child, over the same fixed column.**
///
/// The retirement goes through `Handle::ask`, whose answer the loop sends only after the pump that
/// folded it has published, so the call after it reads the retired snapshot. The demotion is then
/// observed by polling with no `Ask` between the child's end and the answer, for this file's
/// header's reason.
#[test]
fn a_retired_agent_with_a_recorded_child_keeps_its_column_until_that_child_ends() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let (column, mut running, agent, token, host) =
        on_an_injected_column(&fixture, ScriptedRunner::default());
    let turn = fixture.await_turn();

    let mut client = Client::connect(settings.socket()).expect("connect to the daemon");
    let ping = |client: &mut Connection| {
        client
            .call(Method::SystemPing, Some(json!({ "token": token.expose() })))
            .expect("answered")
    };
    assert!(ping(&mut client).error.is_none(), "control: row 2 column 1");

    let retired = running.handle.ask(Ask::Retire(Box::new(RetireParams {
        agent,
        reason: "the quarter ended".to_owned(),
    })));
    assert!(matches!(retired, Ok(Answered::Done)), "{retired:?}");
    let during = ping(&mut client);
    assert!(
        during.error.is_none(),
        "a retired agent whose turn child still runs keeps its column: {:?}",
        during.error
    );

    common::end_turn_with_native(&fixture, &running.handle, &host, turn);
    fixture.await_record("TurnEnded", |record| match record {
        Record::TurnEnded { turn: ended, .. } if *ended == turn => Some(()),
        _ => None,
    });
    await_demotion(&mut client, &token);

    assert_eq!(column.asked(), 1, "resolved once for the connection");
    running.stop(ExitCause::Ended);
}
