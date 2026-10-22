//! Every method the surface registers is pinned to the requirement it was registered with.
//!
//! # Why a whole test file for one `assert_eq!`
//!
//! `register_all` is the only place a requirement is written, which is what stops the method
//! requirements drifting — and is also what made a *downgrade* invisible. Every other test of this
//! surface builds its server behind a seam that resolves an operator, so widening a row to
//! [`Requirement::AnyRole`] — the widest, which admits `PeerRole::Unproven` — changes nothing any
//! of them can observe. Widening a registered method to `AnyRole` while every other
//! test stays green is the failure this file exists to catch; only `loop.pause` would redden
//! without it, because `tests/seam.rs` happens to call that one from a peer the seam does not
//! classify as an operator.
//!
//! `journal.tail` is why that gap was worth closing rather than noting: it streams every `Entry`
//! the daemon appends, `Record::Steer { text }` included, so `AnyRole` on that row hands the
//! journal to a peer that presented nothing — which is where classification puts a peer that
//! presents nothing.
//!
//! # What this does and does not assert
//!
//! The *declaration*, not the enforcement. That the declared requirement is what refuses is
//! `dispatch.rs`'s `the_requirement_is_what_refuses_once_the_seam_has_admitted`; that it refuses a
//! real peer over a real socket is `seam.rs`'s
//! `an_operator_method_is_refused_for_a_caller_the_seam_does_not_classify_as_operator`.
//!
//! The row numbers stay at the registration and are deliberately not repeated here: a second copy
//! of the registration table's rows is a second thing to keep true. What is repeated is the method list, which
//! is the point — a row that loses its registration fails here, and so does a row that gains one,
//! because what is folded is [`Dispatcher::methods`] and not the list itself.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{FakeControl, FakeJournal};
use dudes_proto::rpc::Method;
use dudes_rpc::testing::AdmitAll;
use dudes_rpc::{Dispatcher, Requirement, Subscribers};

/// Every method `dudes_rpc::methods::register_all` registers, with the
/// requirement it is registered with.
///
/// **A bare list of methods and a single `Operator` assertion is not enough here.** `agent.ask` is the first `Agent` registration in the
/// tree, so *"every registered method requires an operator"* is now a false generalisation rather
/// than a description — and a table of pairs is what keeps the downgrade this file exists to catch
/// visible on both rows: widening `agent.ask` to `OperatorOrAgent` is exactly as invisible to every
/// other test as widening an operator row to `AnyRole` was.
const REGISTERED: &[(Method, Requirement)] = &[
    (Method::AgentHire, Requirement::Operator),
    (Method::AgentRetire, Requirement::Operator),
    (Method::AgentRename, Requirement::Operator),
    (Method::AgentLook, Requirement::Operator),
    (Method::AgentMcp, Requirement::Operator),
    (Method::WorkspaceTrust, Requirement::Operator),
    (Method::WorktreeList, Requirement::Operator),
    (Method::WorktreeRemoved, Requirement::Operator),
    (Method::AgentWhy, Requirement::Operator),
    (Method::GoalSet, Requirement::Operator),
    (Method::GoalReplace, Requirement::Operator),
    (Method::LoopPause, Requirement::Operator),
    (Method::LoopResume, Requirement::Operator),
    (Method::LoopHalt, Requirement::Operator),
    (Method::JournalTail, Requirement::Operator),
    (Method::AgentAsk, Requirement::Agent),
];

/// A dispatcher carrying the whole surface.
///
/// The authorizer is [`AdmitAll`] and it is irrelevant to what is asserted: a requirement is a
/// property of the registration, not of the peer, and this file never dispatches anything. It is
/// the one that classifies nobody, so nothing here needs the operator fake.
fn registered() -> Dispatcher {
    let mut dispatcher = Dispatcher::new(AdmitAll);
    dudes_rpc::methods::register_all(
        &mut dispatcher,
        &FakeControl::new(),
        &FakeJournal::empty(),
        &Arc::new(Subscribers::new()),
    );
    dispatcher
}

/// Every registered method carries its expected requirement — and the walk is the
/// **dispatcher's**, so a row added to `register_all` and not to [`REGISTERED`] fails here.
///
/// **Set equality in both directions, and the second direction is the easy one to
/// lack.** Iterating the hand-written table and looking each row up can see a row *changed* or
/// *lost*; it cannot see a row *added*, because nothing iterates the thing that grew. Iterating only the table,
/// appending a second agent-callable registration to `register_all` and not to
/// [`REGISTERED`] left the whole workspace green. So [`Dispatcher::methods`] is what is folded, and
/// [`REGISTERED`] is the declaration it is checked against.
#[test]
fn every_registered_method_carries_a_requirement() {
    let dispatcher = registered();

    let declared: BTreeMap<Method, Requirement> = REGISTERED.iter().copied().collect();
    let actual: BTreeMap<Method, Requirement> = dispatcher
        .methods()
        .map(|method| {
            let requires = dispatcher.requirement(method).unwrap_or_else(|| {
                panic!("{} is registered, so it has a requirement", method.name())
            });
            (method, requires)
        })
        .collect();

    assert_eq!(
        actual, declared,
        "register_all's rows and this file's table are the same set of (method, requirement) \
         pairs; a row added, lost or re-graded on either side is this difference"
    );
    assert_eq!(
        actual.len(),
        REGISTERED.len(),
        "control: the walk saw every row, so the equality above is not two empty maps"
    );
}

/// `agent.ask` is `Agent` and not `OperatorOrAgent`, and it is the only registration that is not
/// `Operator`.
///
/// **Two claims the equality above cannot make.** The first is this: an
/// operator peer able to call `agent.ask` could park any agent in `Wait` without holding that
/// agent's token, which is why the row has no `Operator` half — and a table that merely *agreed
/// with itself* would pass whatever both sides said. The second is the census, and it too is taken
/// **from the dispatcher**: the day a second method becomes agent-callable, this is the file that
/// says so, whether or not anyone remembered to write it down here.
///
/// Each requirement is counted rather than only the one under test, so this could not stay green
/// if every other row became `Agent` as well.
#[test]
fn agent_ask_is_the_only_agent_row_and_it_admits_no_operator() {
    let dispatcher = registered();
    assert_eq!(
        dispatcher.requirement(Method::AgentAsk),
        Some(Requirement::Agent)
    );
    assert_ne!(
        Requirement::Agent,
        Requirement::OperatorOrAgent,
        "control: the two requirements are different values, so the assertion above is a choice"
    );

    let by_requirement = |wanted: Requirement| -> Vec<&'static str> {
        dispatcher
            .methods()
            .filter(|method| dispatcher.requirement(*method) == Some(wanted))
            .map(Method::name)
            .collect()
    };

    assert_eq!(
        by_requirement(Requirement::Agent),
        vec!["agent.ask"],
        "`agent.ask` is the only agent-callable registration; a second one is a method-requirement change"
    );
    assert!(
        by_requirement(Requirement::OperatorOrAgent).is_empty(),
        "and nothing here is registered to both roles: {:?}",
        by_requirement(Requirement::OperatorOrAgent)
    );
    assert!(
        by_requirement(Requirement::AnyRole).is_empty(),
        "nor to every role — `system.ping` is registered by the daemon, not by this function: {:?}",
        by_requirement(Requirement::AnyRole)
    );
    assert_eq!(
        by_requirement(Requirement::Operator).len(),
        15,
        "control: the fifteen operator rows are still operator rows, so the counts above partition \
         the surface rather than reading as an empty filter"
    );
}

/// The control for both tests above: a method nobody registered has no requirement at all, so they
/// are reading the registrations rather than answering `Some(..)` for any name they are handed.
#[test]
fn an_unregistered_method_has_no_requirement() {
    let dispatcher = registered();
    assert_eq!(dispatcher.requirement(Method::SystemDoctor), None);
    assert!(
        !dispatcher
            .methods()
            .any(|method| method == Method::SystemDoctor),
        "and it is absent from the walk as well as from the lookup, which is what makes the walk \
         above a census rather than a re-reading of the same table"
    );
}
