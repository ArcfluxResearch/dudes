//! Every method in `Method::ALL` is routed by the daemon, or is exempt.
//!
//! # The failure this catches
//!
//! A wire contract that names a method the daemon never registers is a promise nothing can break,
//! because nothing keeps it. A method can be missing in two different ways:
//!
//! - **Never registered.** Declared in the method table without any registration.
//! - **Registered only in tests.** A `register(Method::SystemDoctor, ...)` call that sits inside a
//!   `#[cfg(test)] mod tests` in `crates/dudes-rpc/src/dispatch.rs`. Grep for the method and it
//!   looks routed; the daemon never registers it.
//!
//! The second is the one that makes a hand-run check useless, and it is why the routed set below
//! is not read out of the source text.
//!
//! # Where each of the two sets comes from
//!
//! **Declared** is [`Method::ALL`] — the slice the `names!` macro writes from the same block that
//! declares every row, in `crates/dudes-proto/src/rpc/mod.rs`. It is the table, not a copy of it: a
//! row added to that block is in `ALL` in the same edit, so this file cannot fall behind the
//! contract without turning red first. A hand-maintained list of the thirty-six names would be
//! the defect wearing the fix's clothes.
//!
//! **Routed** is [`Dispatcher::methods`] over the dispatcher [`duded::boot::dispatch`] actually
//! builds. That is the daemon's own — the one `Dispatcher::new` across `crates/` and `bins/` that
//! is not inside a test target or a `#[cfg(test)]` module — and it covers every part of the
//! registration surface: `dudes_rpc::methods::register_all` writes nine ordinary rows,
//! `register_agent_effects` writes two conditioned agent rows, `register_memory_effect` writes one
//! conditioned memory row, the hook module adds `hook.event`, and `dispatch` adds `agent.list`,
//! `system.ping`, `system.doctor`, `system.shutdown`, `floor.session.issue` and
//! `floor.enrollment.reload` itself. `board::register` adds the four board rows. Only a test in this package can see all parts, which is why the file lives here
//! and not beside `register_all`.
//!
//! **"Routed" here means registered, which is weaker than reachable.** The census splits every
//! declared method into registered and exempt. Which caller reaches a route remains the
//! classifier's and the registered requirement's decision.
//!
//! # How `#[cfg(test)]` registrations are excluded
//!
//! By compilation, not by a heuristic. This is an integration test, so it links `dudes-rpc` built
//! **without** `cfg(test)`: `dispatch.rs`'s two test-only `Method::SystemDoctor` registrations are
//! not in the binary under test, and therefore cannot satisfy the production-positive assertion.
//! Contrast
//! `crates/dudes-rpc/tests/no_operator_fake.rs`, which must cut each source file at a
//! `\n#[cfg(test)]` marker and carries the residuals of doing so. Nothing of that shape is needed,
//! and [`system_doctor_is_operator_only_on_the_daemons_own_dispatcher`] pins the difference on the
//! very method that motivated it.
//!
//! # What this file would not notice
//!
//! Three things, and the first is a **silent green** rather than a noisy red.
//!
//! **A second production dispatcher.** [`routed`] folds `boot::dispatch` and nothing else, so a
//! registration made elsewhere is invisible to it. That blind spot is avoided by putting
//! `hook.event` on this same dispatcher; the dedicated assertion below pins both its location and
//! its `Agent` requirement.
//!
//! **Authorisation.** The general fold is over *registration*, and a registration carries a
//! [`dudes_rpc::Requirement`] that census does not read: registering `Method::MemoryRead` in
//! `boot::dispatch` as `Requirement::AnyRole` and deleting its exemption passes there, and
//! `Requirement::admits` matches `(Self::AnyRole, _)` — every role, `PeerRole::Unproven` included
//! (`crates/dudes-rpc/src/auth.rs:481`, with row 4 minting `Unproven` at `:638`). The live
//! `register_all` surface is guarded as a set by `crates/dudes-rpc/tests/requirements.rs`; the
//! production-dispatch assertion here additionally pins `system.doctor` to its exact
//! `Requirement`.
//!
//! **Reachability** — see [`routed`], which says what "routed" does and does not mean.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;

use common::{Fixture, TestClock};
use duded::classify::WorldTokens;
use duded::ports::{Handle, Job};
use dudes_proto::rpc::cred::{OPERATOR_CREDENTIAL_LEN, OperatorCredential};
use dudes_proto::rpc::{CurrentMethod, Method};
use dudes_rpc::auth::{PeerClassifier, PeerRole};
use dudes_rpc::{Dispatcher, Requirement, Subscribers};
use dudes_types::AgentId;

/// The methods the contract declares and the daemon does not register.
///
/// **An exemption is a debt, not a decision.** Eight of the contract's
/// rows are here, which is the honest state of the surface and not a design: the
/// list exists so that the number is visible rather than discovered by an operator
/// whose call comes back `method not found`.
///
/// A row leaves this list by being routed, and [`no_exemption_outlives_its_route`] is what makes
/// leaving compulsory: an entry for a method that has since been registered fails the suite. That
/// is the direction of a constant with no producer, and it is why the list cannot
/// quietly accumulate rows that stopped being true.
///
/// **That compulsion reaches exactly as far as [`routed`] sees, which is `boot::dispatch` and
/// nothing else.** A row whose method gets registered on some other production dispatcher stays
/// green; the dedicated `hook.event` assertion above is why that route cannot move there silently.
const UNROUTED: &[Method] = &[
    Method::AgentGet,
    Method::GoalClear,
    Method::GoalReassign,
    Method::PtyResize,
    Method::MailSend,
    Method::JournalReplay,
    Method::FloorSubscribe,
    Method::FloorUnsubscribe,
];

#[test]
fn memory_read_carries_the_existing_agent_or_operator_grant() {
    assert_eq!(
        production_dispatcher().requirement(Method::MemoryRead),
        Some(Requirement::OperatorOrAgent)
    );
}

#[test]
fn memory_search_carries_the_agent_or_operator_grant() {
    assert_eq!(
        production_dispatcher().requirement(Method::MemorySearch),
        Some(Requirement::OperatorOrAgent)
    );
}

/// A credential of the right shape. Irrelevant to what is asserted — nothing here presents
/// anything — but [`PeerClassifier`] refuses to be built without one, and a requirement is a
/// property of the registration rather than of the peer.
fn credential() -> OperatorCredential {
    OperatorCredential::new("c".repeat(OPERATOR_CREDENTIAL_LEN)).expect("a credential")
}

/// Every method the daemon's production dispatcher registers.
///
/// **Registered, which is neither *reachable* nor *authorised*, and this set is used as neither.**
/// A method is in here once `boot::dispatch` has called `register` for it: it says nothing about
/// the [`dudes_rpc::Requirement`] that registration carries, and nothing about whether any peer can
/// satisfy it. The census does not replace authority or live-peer tests.
///
/// Two more limits worth naming here, because this function is where they bite:
/// **`boot::dispatch` is the only dispatcher folded**, so a registration made on another one is
/// invisible; and the fold is of a dispatcher built from a throwaway [`Fixture`], which is
/// legitimate only because `register` takes the method and the requirement and never consults the
/// settings, the journal or the loop.
///
/// The receiver is held for the call's duration rather than dropped: a `Handle` over a closed
/// channel is a live object whose sends fail, and nothing about this should turn on which.
fn production_dispatcher() -> Dispatcher {
    let fixture = Fixture::new();
    let (jobs, _loop_side) = mpsc::channel::<Job>();
    duded::boot::dispatch(
        &fixture.settings(),
        PeerClassifier::new(credential(), Arc::new(WorldTokens::new())),
        duded::hooks::Ingress::new(Handle::new(jobs), Arc::new(TestClock::new())),
        &fixture.journal_dir(),
        &Arc::new(Subscribers::new()),
        Arc::new(AtomicU64::new(0)),
        Arc::new(dudes_rpc::OperatorAuthoritySource::from_replayed_agents(
            false,
        )),
    )
}

fn routed() -> BTreeSet<Method> {
    production_dispatcher().methods().collect()
}

/// The methods [`UNROUTED`] exempts.
fn exempt() -> BTreeSet<Method> {
    UNROUTED.iter().copied().collect()
}

/// Every declared method is registered by the daemon or is on [`UNROUTED`].
///
/// **Deleting one row from [`UNROUTED`] fails here**, which is the property the file exists for:
/// the exemption list cannot be trimmed without either routing the method or being told. So does
/// adding a row to the contract's `names!` block, because what is folded is [`Method::ALL`] and not
/// a copy of it.
///
/// Three controls stop this from passing over degenerate sets. The census equality holds the whole
/// partition. `agent.hire` must be routed — a fold that saw an *empty* dispatcher would otherwise
/// pass if [`UNROUTED`] had grown to thirty-six rows — and `exempt.len() == UNROUTED.len()` is what
/// stops a deleted row hiding behind a duplicate of itself.
#[test]
fn every_declared_method_is_routed_or_exempt() {
    let routed = routed();
    let exempt = exempt();

    let unaccounted: Vec<&str> = Method::ALL
        .iter()
        .filter(|method| !routed.contains(method) && !exempt.contains(method))
        .map(|method| method.name())
        .collect();
    assert!(
        unaccounted.is_empty(),
        "declared on the contract, registered by no production `register` call, and on no \
         exemption: {unaccounted:?}. Route it, or add it to UNROUTED."
    );

    assert_eq!(
        routed.len() + exempt.len(),
        Method::ALL.len(),
        "the routed and exempt sets together account for every declared method exactly once"
    );

    assert_eq!(
        exempt.len(),
        UNROUTED.len(),
        "control: UNROUTED names each method once, so deleting a row cannot hide behind a twin"
    );
    assert!(
        routed.contains(&Method::AgentHire),
        "control: the fold reached a real dispatcher, so the pass above is not a fold over nothing"
    );
}

#[test]
fn the_exported_current_catalog_matches_the_production_dispatcher() {
    let rpc = dudes_proto::schema::schemas()
        .into_iter()
        .find(|document| document.file_name == "rpc.schema.json")
        .unwrap()
        .schema;
    assert_eq!(
        rpc["properties"]["methods"]["$ref"],
        "#/$defs/CurrentMethods"
    );
    let declared: BTreeSet<CurrentMethod> = rpc["$defs"]["CurrentMethods"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(|name| name.parse().unwrap())
        .collect();
    let dispatcher = production_dispatcher();
    let routed: BTreeSet<_> = dispatcher.current_methods().collect();
    let exempt: BTreeSet<_> = UNROUTED.iter().copied().map(CurrentMethod::from).collect();
    assert!(routed.is_disjoint(&exempt));
    assert_eq!(declared, routed.union(&exempt).copied().collect());
    assert_eq!(
        dispatcher.current_requirement(CurrentMethod::TicketCheckout),
        Some(Requirement::Operator)
    );
    assert_eq!(
        dispatcher
            .methods()
            .map(CurrentMethod::from)
            .collect::<BTreeSet<_>>(),
        routed
            .iter()
            .copied()
            .filter(|method| method.legacy().is_some())
            .collect()
    );
}

#[test]
fn checkout_requires_the_operator_connections_peer_pid() {
    use dudes_proto::rpc::{ErrorCode, JSONRPC, Request, RequestId};
    use dudes_rpc::Outbound;
    use dudes_rpc::auth::PeerProvenance;

    let request = Request {
        jsonrpc: JSONRPC.to_owned(),
        id: RequestId::Number(1),
        method: CurrentMethod::TicketCheckout.name().to_owned(),
        params: Some(serde_json::json!({"card": dudes_types::CardId::NIL, "expected_seq": 0})),
        v: None,
        cred: Some(credential()),
    };
    let response = production_dispatcher().dispatch(
        &request,
        PeerProvenance::NoRecordedAgent,
        &Arc::new(Outbound::sink()),
    );
    let refusal = response.error.unwrap();
    assert_eq!(refusal.code, ErrorCode::Unauthorized.code());
    assert_eq!(refusal.message, "board caller has no peer pid");
}

/// `system.doctor` is Operator-only on the daemon's own dispatcher.
///
/// The assertion is deliberately over [`routed`], which constructs `boot::dispatch`: registering
/// the method on a test-only or second production dispatcher does not satisfy it. The exact
/// requirement is a property of that production registration, not an inference from which callers
/// the live test happens to exercise. `system.ping` is the control that the fold reached the
/// daemon's dispatcher at all.
#[test]
fn system_doctor_is_operator_only_on_the_daemons_own_dispatcher() {
    let dispatcher = production_dispatcher();
    let routed: BTreeSet<_> = dispatcher.methods().collect();
    assert!(
        routed.contains(&Method::SystemDoctor),
        "system.doctor must be registered by boot::dispatch, not only by another dispatcher"
    );
    assert_eq!(
        dispatcher.requirement(Method::SystemDoctor),
        Some(Requirement::Operator),
        "production system.doctor is Operator-only"
    );
    assert!(
        routed.contains(&Method::SystemPing),
        "control: a method the daemon does register is seen, so the assertion above is an absence \
         and not an empty dispatcher"
    );
}

/// `agent.list` is Operator-only on the daemon's own dispatcher: an agent's token does not read the
/// roster.
#[test]
fn agent_list_is_operator_only_on_the_daemons_own_dispatcher() {
    let dispatcher = production_dispatcher();
    let requirement = dispatcher
        .requirement(Method::AgentList)
        .expect("agent.list must be registered by boot::dispatch");
    assert_eq!(
        requirement,
        Requirement::Operator,
        "roster is operator reconnaissance"
    );
    let agent = PeerRole::Agent {
        agent: AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("an agent id"),
    };
    assert!(
        !requirement.admits(&agent),
        "an agent token is refused agent.list"
    );
    assert!(
        dispatcher
            .requirement(Method::HookEvent)
            .is_some_and(|hook| hook.admits(&agent)),
        "control: the same role is admitted where an agent may call"
    );
}

/// `hook.event` is an Agent-only row on the daemon's one production dispatcher.
#[test]
fn hook_event_is_agent_only_on_the_daemons_own_dispatcher() {
    let dispatcher = production_dispatcher();
    assert!(
        dispatcher
            .methods()
            .any(|method| method == Method::HookEvent),
        "hook.event must be registered by boot::dispatch"
    );
    assert_eq!(
        dispatcher.requirement(Method::HookEvent),
        Some(Requirement::Agent),
        "the token identifies a candidate agent and structural provenance qualifies it"
    );
}

/// A retired agent's still-recorded child keeps resolving to its
/// agent, and each of these methods either acts on nothing or decides on the loop thread what
/// that child gets. Another method admitting `Agent` needs the same decision.
#[test]
fn the_agent_reachable_methods_match_the_retirement_decisions() {
    let dispatcher = production_dispatcher();
    let agent = PeerRole::Agent {
        agent: AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("an agent id"),
    };
    let reachable: BTreeSet<Method> = Method::ALL
        .iter()
        .copied()
        .filter(|method| {
            dispatcher
                .requirement(*method)
                .is_some_and(|requirement| requirement.admits(&agent))
        })
        .collect();
    assert_eq!(
        reachable,
        BTreeSet::from([
            Method::SystemPing,
            Method::SystemClosingTime,
            Method::HookEvent,
            Method::AgentAsk,
            Method::MemoryAppend,
            Method::MemoryRead,
            Method::MemorySearch,
            Method::BoardClaim,
            Method::BoardTransition,
            Method::TeamRoster,
        ]),
        "a new method that admits an agent must decide on the loop thread what a \
         retired agent's recorded child gets, or be kept from the Agent role"
    );
}

#[test]
fn transport_pid_admission_covers_agent_grants_and_keeps_ping_liveness() {
    let dispatcher = production_dispatcher();
    let guarded: BTreeSet<_> = dispatcher
        .methods()
        .filter(|method| {
            matches!(
                dispatcher.requirement(*method),
                Some(Requirement::Agent | Requirement::OperatorOrAgent)
            )
        })
        .collect();
    assert_eq!(
        guarded,
        BTreeSet::from([
            Method::AgentAsk,
            Method::HookEvent,
            Method::MemoryAppend,
            Method::MemoryRead,
            Method::MemorySearch,
            Method::BoardClaim,
            Method::BoardTransition,
            Method::SystemClosingTime,
            Method::TeamRoster,
        ])
    );
    assert_eq!(
        dispatcher.requirement(Method::SystemPing),
        Some(Requirement::AnyRole)
    );
}

/// No exemption outlives the route it was waiting for.
///
/// The other direction: a constant with no producer. Once a method is
/// registered its `UNROUTED` row is a debt that has been paid and is still on the books, and a
/// stale row is worse than none — it is a written claim that the method is unrouted, which the next
/// reader has no reason to doubt. So the list clears itself: routing an exempted method turns
/// this test red until its row goes.
#[test]
fn no_exemption_outlives_its_route() {
    let routed = routed();
    let stale: Vec<&str> = UNROUTED
        .iter()
        .filter(|method| routed.contains(method))
        .map(|method| method.name())
        .collect();
    assert!(
        stale.is_empty(),
        "exempted and also registered: {stale:?}. The route landed; delete the UNROUTED row."
    );

    assert!(
        !UNROUTED.is_empty() && !routed.is_empty(),
        "control: both sides are non-empty, so the disjointness above is a finding and not an \
         empty intersection with an empty set"
    );
}
