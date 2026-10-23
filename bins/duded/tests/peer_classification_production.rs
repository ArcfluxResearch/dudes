//! The peer-classification table, every cell, against the **production** pair.
//!
//! # What "the production pair" means here, and what it does not
//!
//! Every part of the decision below is the shipped one:
//!
//! - the classifier is `dudes_rpc::PeerClassifier`, the type `duded::boot` constructs;
//! - the credential comparison is the real one, against a real `OperatorCredential`;
//! - the token projection is `duded::classify::WorldTokens`, the shipped `MintedTokens` adapter,
//!   doing the shipped hashing — and the map it publishes is `World::token_hash_owners()` over a
//!   `World` folded from real journal records, not a hand-written table.
//!
//! `crates/dudes-rpc/tests/peer_classification_cells.rs` asserts the same grid over `classify` and a fake
//! projection. That file is the grid; this one is the grid *as the daemon computes it*, and the
//! two differ in exactly the places a fake could have been wrong: which row a presented credential
//! or token lands in.
//!
//! **What is not production here is the column.** It is named, one value per cell, because that is
//! what `Authorizer::authorize` now takes — the transport resolves it once per connection, and the
//! daemon's own resolver pins every connection to `Unresolvable`. So this file is evidence about
//! the sixteen representative cells and none at all about which column a real peer reaches;
//! `bins/duded/tests/wire.rs` and `bins/dudes/tests/doors.rs` are the arms that cross a socket.
//!
//! # No socket, so no `cfg`
//!
//! Nothing here binds anything, which is deliberate: these cells were invisible on the Windows leg
//! of the matrix for as long as they shared a file with an arm that needed a Unix socket.

use std::sync::Arc;

use duded::classify::WorldTokens;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::hook::{HOOK_TOKEN_LEN, HookToken};
use dudes_proto::{AgentBudget, Entry, Record};
use dudes_rpc::auth::{Authorizer, PeerClassifier, PeerProvenance, PeerRole, Presented, Refusal};
use dudes_supervisor::{World, blake3_hex};
use dudes_types::{AgentId, Lsn, Provider, Timestamp, Tokens, Usd};

/// The agent whose token every row-2 cell presents.
fn alpha() -> AgentId {
    AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a ulid")
}

/// A *different* agent — the sibling whose tree a stolen token gets called from.
fn bravo() -> AgentId {
    AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAY").expect("a ulid")
}

/// Alpha's token. Distinctive, so a projection answering for any token could not pass for one
/// answering for this one.
const ALPHA_TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// A token no minting names.
const UNMINTED_TOKEN: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";
/// This daemon's operator credential.
const CREDENTIAL: &str = "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";

fn token(text: &str) -> HookToken {
    assert_eq!(text.len(), HOOK_TOKEN_LEN, "the fixture is well formed");
    HookToken::new(text).expect("a token")
}

fn credential() -> OperatorCredential {
    OperatorCredential::new(CREDENTIAL).expect("a credential")
}

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_000),
        record,
    }
}

/// A `World` that has folded alpha's hire and the minting of [`ALPHA_TOKEN`] — the same two
/// records a running daemon folds before an agent's first spawn.
fn folded() -> World {
    let mut world = World::default();
    world.apply(
        Lsn(1),
        &entry(Record::AgentHired {
            character: None,
            accent: None,
            id: alpha(),
            name: "Kim".to_owned(),
            role: "backend engineer".to_owned(),
            department: "engineering".to_owned(),
            provider: Provider::Claude,
            cwd: "/tmp/dudes-fixture".into(),
            budget: AgentBudget {
                usd_per_day: Some(Usd::from_dollars(10)),
                tokens_per_day: Some(Tokens::new(1_000)),
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            model: None,
            trusted: false,
            workspace: None,
            worktree: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        }),
    );
    world.apply(
        Lsn(2),
        &entry(Record::HookTokenMinted {
            agent: alpha(),
            token_hash: blake3_hex(ALPHA_TOKEN.as_bytes()),
        }),
    );
    world
}

/// The shipped classifier over the shipped adapter, holding what the shipped fold produced.
fn classifier() -> PeerClassifier {
    let tokens = WorldTokens::new();
    tokens.publish(folded().token_hash_owners());
    PeerClassifier::new(credential(), Arc::new(tokens))
}

/// What a cell resolves to, flattened so the grid reads as a grid.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Operator,
    Agent(AgentId),
    Unproven,
    /// `-32001 Unauthorized`.
    Unauthorized,
    /// `-32004 Refused{gate: token}`.
    RefusedToken,
}

impl Verdict {
    fn of(result: &Result<PeerRole, Refusal>) -> Self {
        match result {
            Ok(PeerRole::Operator) => Self::Operator,
            Ok(PeerRole::Agent { agent }) => Self::Agent(*agent),
            Ok(PeerRole::Unproven) => Self::Unproven,
            Err(Refusal::Unauthorized(_)) => Self::Unauthorized,
            Err(Refusal::Token) => Self::RefusedToken,
        }
    }
}

/// The classification table's columns that these tests present — column 3 twice, because two
/// provenances land there and the table gives them one answer.
fn columns() -> [(&'static str, PeerProvenance); 4] {
    [
        (
            "col 1 · ancestry reaches agent A's recorded child",
            PeerProvenance::Agent(alpha()),
        ),
        (
            "col 2 · ancestry reaches a different agent's child",
            PeerProvenance::Agent(bravo()),
        ),
        (
            "col 3 · walked cleanly to no recorded agent child",
            PeerProvenance::NoRecordedAgent,
        ),
        (
            "col 3 · unresolvable, because the process was gone",
            PeerProvenance::Unresolvable,
        ),
    ]
}

/// One row of the grid: what it is called, what a call in it presents, and its four verdicts.
///
/// The presentation is a function rather than a value because [`Presented`] is not `Clone` — the
/// credential deliberately has no `Clone` — and each of the four representative values needs its
/// own.
type Row = (&'static str, fn() -> Presented, [Verdict; 4]);

/// What each row *presents*, which is where this file differs from the crate-level grid: the row
/// is not named, it is derived — by the production credential comparison and the production
/// projection — from what the call carries.
fn rows() -> [Row; 4] {
    [
        (
            "row 1 · this daemon's operator credential",
            || Presented {
                token: None,
                cred: Some(credential()),
            },
            [
                Verdict::Unauthorized,
                Verdict::Unauthorized,
                Verdict::Operator,
                Verdict::Operator,
            ],
        ),
        (
            "row 2 · a token the fold names a non-retired agent for",
            || Presented {
                token: Some(token(ALPHA_TOKEN)),
                cred: None,
            },
            [
                Verdict::Agent(alpha()),
                Verdict::Unauthorized,
                Verdict::Unauthorized,
                Verdict::Unauthorized,
            ],
        ),
        (
            "row 3 · a token no minting names",
            || Presented {
                token: Some(token(UNMINTED_TOKEN)),
                cred: None,
            },
            [
                Verdict::RefusedToken,
                Verdict::RefusedToken,
                Verdict::RefusedToken,
                Verdict::RefusedToken,
            ],
        ),
        (
            "row 4 · nothing",
            Presented::default,
            [
                Verdict::Unproven,
                Verdict::Unproven,
                Verdict::Unproven,
                Verdict::Unproven,
            ],
        ),
    ]
}

/// Sixteen representative cells, decided by the daemon's own classifier
/// over the daemon's own credential and the daemon's own token projection.
///
/// The two controls the crate-level grid names are both inside this one, and they are what stop it
/// passing vacuously:
///
/// - **Row 2 column 1 is `Agent(alpha)`.** It can only be reached if `WorldTokens` hashed the
///   presented token the way the mint did *and* `World::token_hash_owners` carried that hash. A
///   projection that named nobody would send this row to row 3's verdicts and redden the whole row.
/// - **Row 1 column 3 is `Operator`.** Without it, a classifier that refused every credential
///   would satisfy the refusing cells perfectly.
#[test]
fn every_cell_of_the_grid_resolves_as_asserted_against_the_production_pair() {
    let seam = classifier();
    for (row_name, present, expected) in rows() {
        for ((column_name, provenance), expected) in columns().into_iter().zip(expected) {
            assert_eq!(
                Verdict::of(&seam.authorize(present(), provenance, "memory.append")),
                expected,
                "{row_name} / {column_name}"
            );
        }
    }
}

/// A retired agent's token leaves the projection once it has no recorded child, as this fold's
/// agent never had one, so the same presentation moves from row 2 to row 3 — the whole grid above
/// shifts under it, and this is the one cell that says so.
///
/// The live arm is the control: without it an empty projection would satisfy the retired arm.
#[test]
fn retiring_the_agent_moves_its_token_from_row_two_to_row_three() {
    let live = classifier();
    let presented = || Presented {
        token: Some(token(ALPHA_TOKEN)),
        cred: None,
    };
    assert_eq!(
        Verdict::of(&live.authorize(presented(), PeerProvenance::Agent(alpha()), "memory.append")),
        Verdict::Agent(alpha()),
        "control: the token is honoured while the agent is live"
    );

    let mut world = folded();
    world.apply(
        Lsn(3),
        &entry(Record::AgentRetired {
            id: alpha(),
            reason: "the quarter ended".to_owned(),
            browser_retire: None,
        }),
    );
    let tokens = WorldTokens::new();
    tokens.publish(world.token_hash_owners());
    let retired = PeerClassifier::new(credential(), Arc::new(tokens));

    assert_eq!(
        Verdict::of(&retired.authorize(
            presented(),
            PeerProvenance::Agent(alpha()),
            "memory.append"
        )),
        Verdict::RefusedToken,
        "the same token, the same column: retirement is what moved the row"
    );
}
