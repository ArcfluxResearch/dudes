//! `cargo xtask layering` — the CI gate for invariant 9.
//!
//! > `dudes-supervisor` depends on no data-plane crate (`dudes-pty`, `dudes-proxy`).
//! > `dudes-hook` links no async runtime, no TLS, no subscriber.
//!
//! The rules are declared here, checked over the resolved `cargo metadata` graph (all features,
//! dev-dependencies excluded). A rule whose member crate does not exist yet is reported as skipped
//! and passes: the gate is live from the first commit and tightens as crates land. `--strict` turns
//! a skip into a failure; CI flips to it once every member crate exists.
//!
//! The gate reads the table in two directions, and they have deliberately different lifecycles:
//!
//! - **Every workspace member has a row** ([`uncovered_members`]) — enforced always. A new crate must add its row; that is
//!   a check, not prose, so a crate cannot land unenforced.
//! - **Every row has a workspace member** ([`verdict`] under `--strict`) — enforced on request,
//!   because a row for a crate that has not been written yet is how the gate stays ahead of the
//!   code.
//!
//! Shipped binary closures also reject known network clients and outbound source capabilities.

use std::collections::BTreeSet;

use anyhow::{Result, bail};

use crate::metadata::{self, Graph};

#[path = "outbound.rs"]
mod outbound;

/// A layering rule: `member` must not transitively depend on any of `forbidden`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rule {
    pub(crate) member: &'static str,
    pub(crate) forbidden: &'static [&'static str],
    pub(crate) why: &'static str,
}

/// The layering rules of invariant 9. New crates add rows here.
///
/// Adding a row is mandatory, not conventional: [`uncovered_members`] fails the gate for any
/// workspace member with no row. Every entry of every row is mirrored in the test module's
/// `PINNED` table, which is asserted set-equal to this one in both directions — so an entry cannot
/// be added or removed here without the suite noticing.
pub(crate) const RULES: &[Rule] = &[
    Rule {
        member: "dudes-types",
        forbidden: &[
            "tokio",
            "async-std",
            "smol",
            "async-io",
            "tracing",
            "redb",
            // The same two entries `dudes-policy` carries, on the crate that *owns* the fix rather
            // than the crate that observes the breach. `ulid/std` is `dep:rand` (ulid 3.0.0), so
            // forwarding it from this crate's `std` feature is what puts
            // `dudes-types -> ulid -> rand -> getrandom` back in the normal graph — and invariant 5
            // is breached one hop later, at `dudes-policy`. With the entries on `dudes-policy`
            // alone, that reintroduction reddens on a row whose crate is not where the feature is
            // declared; here it also reddens on the manifest that decided it.
            "rand",
            "getrandom",
        ],
        why: "dudes-types is vocabulary: no I/O, no clock, no runtime, no entropy",
    },
    Rule {
        member: "dudes-proto",
        forbidden: &[
            "tokio",
            "async-std",
            "smol",
            "async-io",
            "tracing-subscriber",
            "redb",
            "dudes-journal",
            "dudes-policy",
            "dudes-supervisor",
            "dudes-pty",
            "dudes-proxy",
        ],
        why: "dudes-proto is the contract: it depends on nothing that acts",
    },
    Rule {
        member: "dudes-journal",
        forbidden: &[
            "tokio",
            "async-std",
            "smol",
            "async-io",
            "dudes-policy",
            "dudes-supervisor",
            "dudes-pty",
            "dudes-proxy",
            "dudes-rpc",
            "dudes-providers",
        ],
        why: "dudes-journal is the synchronous writer: it depends on the contract and nothing that acts",
    },
    Rule {
        member: "dudes-policy",
        forbidden: &[
            "tokio",
            "async-std",
            "smol",
            "async-io",
            "tracing",
            "tracing-subscriber",
            "redb",
            "dudes-journal",
            "dudes-supervisor",
            "dudes-pty",
            "dudes-proxy",
            // Not "does it act" but "does it know": an adapter is per-vendor and a transport is
            // per-connection, so reaching either is how `decide()` starts branching on which
            // vendor or which peer it is deciding for. That is not a mechanical breach of
            // invariant 5 — it is the edge along which one arrives.
            "dudes-providers",
            "dudes-rpc",
            // Invariant 5's other half: `decide()` is *reproducible from the journal*, so the loop
            // must not be able to reach a random source. A tie-break, a jitter or a shuffle taken
            // from `rand` would make a decision that replay cannot reproduce, and no other entry in
            // this row is close enough to catch it. `getrandom` is named as well because it is the
            // layer underneath and reachable on its own: `duded` depends on it directly and
            // deliberately (`bins/duded/src/main.rs`, so that minting a token does not rest on
            // whichever crate happened to enable a feature), which is exactly the shape that would
            // otherwise walk into this crate unnoticed.
            "rand",
            "getrandom",
        ],
        why: "invariant 5: the loop is pure — no runtime, no I/O crate, no entropy, nothing that acts or knows a vendor",
    },
    Rule {
        member: "dudes-providers",
        forbidden: &[
            "tokio",
            "async-std",
            "smol",
            "async-io",
            "tracing-subscriber",
            "rustls",
            "native-tls",
            "reqwest",
            "redb",
            "dudes-journal",
            "dudes-policy",
            "dudes-supervisor",
            "dudes-pty",
            "dudes-proxy",
            "dudes-rpc",
        ],
        why: "an adapter builds specs and parses lines; it never acts",
    },
    Rule {
        member: "dudes-rpc",
        forbidden: &[
            "tokio",
            "async-std",
            "smol",
            "async-io",
            "rustls",
            "native-tls",
            "openssl-sys",
            "tracing-subscriber",
            // The door decides nothing, carries no session bytes and owns no child: the loop is
            // decided behind it, and a transport that could reach `decide()` is a transport that
            // could start answering a call without one.
            "dudes-policy",
            "dudes-pty",
            "dudes-proxy",
            "dudes-host",
        ],
        why: "invariant 9: the shim's transport links no runtime, TLS, subscriber or child owner; only the DACL/filesystem leaf is admitted",
    },
    Rule {
        member: "dudes-supervisor",
        forbidden: &[
            "dudes-pty",
            "dudes-proxy",
            // Sans-io is the crate's other rule and it was previously only asserted in prose:
            // `handle()` performs no I/O, so it reaches no journal, no store and no runtime.
            // Effects are values; the runtime that executes them is what may depend on these.
            "dudes-journal",
            "redb",
            "tokio",
            // The real host implements this crate's trait, so the edge runs host -> supervisor and
            // never back. Not "data plane" — it carries no session bytes — which is why the rule
            // names it here rather than the invariant's parenthetical growing another word.
            "dudes-host",
        ],
        why: "invariant 9: the supervisor depends on no data-plane crate, and handle() is sans-io",
    },
    Rule {
        member: "dudes-host",
        forbidden: &[
            // The host's children are never a runtime's to reap. Tokio's process module documents
            // that it reaps what it spawned, on its own schedule — which frees a child's identifier
            // mid-grace behind the host's back and dissolves the reservation the kill path rests on.
            "tokio",
            "async-std",
            "smol",
            "async-io",
            "dudes-journal",
            "redb",
            // `dudes-policy` is deliberately absent, and the omission is load-bearing rather than
            // an oversight. This gate reads *transitive* reachability, and the host implements
            // `dudes-supervisor`'s trait while `dudes-supervisor` depends on `dudes-policy` — so
            // the edge exists for every crate on this side of the seam and the entry could only
            // ever be a false alarm. The supervisor's own row omits it for the same reason. What
            // the entry was meant to catch — the host *deciding* — is caught instead by the two
            // above: a host that decided anything would have to read state to decide it.
            "dudes-pty",
            "dudes-proxy",
            "dudes-rpc",
            "rustls",
            "native-tls",
            "reqwest",
        ],
        why: "the host executes effects synchronously: it owns children, decides nothing, records nothing, \
              and runs no runtime of its own",
    },
    Rule {
        member: "dudes-pty",
        forbidden: &[
            "tokio",
            "async-std",
            "smol",
            "async-io",
            "tracing-subscriber",
            "rustls",
            "native-tls",
            "openssl-sys",
            "reqwest",
            "redb",
            "dudes-journal",
            "dudes-policy",
            "dudes-supervisor",
            "dudes-rpc",
            "dudes-host",
            "dudes-win",
            "dudes-proxy",
            "alacritty_terminal",
            "termwiz",
            "vtparse",
            "wezterm-term",
        ],
        why: "the PTY host owns process trees and relays bytes; it decides, journals, routes and parses nothing",
    },
    Rule {
        member: "dudes-win",
        forbidden: &[
            "tokio",
            "async-std",
            "smol",
            "async-io",
            "tracing",
            "tracing-subscriber",
            "rustls",
            "native-tls",
            "openssl-sys",
            "reqwest",
            "redb",
            "dudes-types",
            "dudes-proto",
            "dudes-journal",
            "dudes-policy",
            "dudes-providers",
            "dudes-rpc",
            "dudes-supervisor",
            "dudes-host",
            "dudes-pty",
            "dudes-proxy",
            "dudes",
            "duded",
        ],
        why: "the Windows DACL/filesystem leaf depends only on windows-sys: no process owner, loop, store, runtime or network",
    },
    Rule {
        member: "duded",
        forbidden: &[
            // The daemon owns children through `dudes-host`, and a runtime that reaped them on its
            // own schedule would free a child's identifier mid-grace behind the host's back.
            // The row is the complement to `bins/duded/clippy.toml`'s
            // `disallowed-types`: that one bans a *type* in this package's own source, this one
            // bans *linking* a crate anywhere in its tree — and neither can express the other.
            "tokio",
            "async-std",
            "smol",
            "async-io",
            // The daemon terminates the control plane; nothing it links reaches the network.
            "rustls",
            "native-tls",
            "openssl-sys",
            "reqwest",
            // The daemon is the composition root for the PTY leaf; the supervisor remains barred
            // from every data-plane crate by its own row.
            "dudes-proxy",
        ],
        why: "the daemon owns every child through dudes-host and injects the PTY leaf: no competing runtime, TLS, or proxy data plane",
    },
    Rule {
        member: "dudes",
        forbidden: &[
            // The CLI is a client. It links the contract and the transport, and the transport is
            // synchronous by invariant 9 — one connection, one frame, one answer — so there is
            // nothing here for a runtime to drive.
            "tokio",
            "async-std",
            "smol",
            "async-io",
            // It reaches one Unix socket on the local machine and nothing else.
            "rustls",
            "native-tls",
            "openssl-sys",
            "reqwest",
            // The four crates that make this row say something the dependency list does not already
            // say by being short. A CLI that linked any of them could journal, decide, own a child
            // or fold a projection in the caller's process — which is invariant 1's "one writer"
            // lost to a second implementation rather than to a second daemon. They are reachable
            // from this package's `[dev-dependencies]`, and that is exactly why the row is worth
            // having: the gate drops dev edges, so the row asserts the *shipped* binary's graph.
            "dudes-journal",
            "dudes-policy",
            "dudes-supervisor",
            "dudes-host",
            // Data-plane crates are the floor's and the daemon's, never the CLI's.
            "dudes-pty",
            "dudes-proxy",
        ],
        why: "the CLI is a client: the contract and the transport, nothing that journals, decides, \
              owns a child or runs a runtime",
    },
    Rule {
        member: "dudes-hook",
        forbidden: &[
            "tokio",
            "async-std",
            "smol",
            "async-io",
            "rustls",
            "native-tls",
            "openssl-sys",
            "tracing-subscriber",
        ],
        why: "invariant 9: the hook links no async runtime, no TLS, no subscriber",
    },
    Rule {
        member: "xtask",
        forbidden: &[
            "tokio",
            "async-std",
            "smol",
            "async-io",
            "rustls",
            "native-tls",
            "openssl-sys",
            "reqwest",
        ],
        why: "repo automation is synchronous and offline: it shells out to cargo and fetches nothing",
    },
];

/// Outcome of one rule.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The member crate is not in the workspace yet.
    Skipped,
    /// The member exists and reaches none of the forbidden crates.
    Ok,
    /// Paths (member → … → forbidden) for every forbidden crate that is reachable.
    Violated(Vec<Vec<String>>),
}

/// Evaluate `rule` against `graph`. Errors only when the graph itself cannot be walked.
pub(crate) fn check(graph: &Graph, rule: &Rule) -> Result<Outcome> {
    let Some(start) = graph.member_id(rule.member) else {
        return Ok(Outcome::Skipped);
    };
    let mut paths = Vec::new();
    let network = outbound::FORBIDDEN_DEPS.iter().filter(|target| {
        outbound::BINARIES.contains(&rule.member) && !rule.forbidden.contains(target)
    });
    for target in rule.forbidden.iter().chain(network) {
        if let Some(path) = graph.path_to(start, target)? {
            paths.push(path);
        }
    }
    Ok(if paths.is_empty() {
        Outcome::Ok
    } else {
        Outcome::Violated(paths)
    })
}

/// Workspace members with no row in [`RULES`], in `workspace_members` order.
///
/// This is the house rule "a new crate adds its row" turned into a check. Without it a crate can
/// land with no layering rule and the gate reports nothing — the gate is green precisely because
/// it is not looking.
///
/// The direction is one-way on purpose: this asserts `workspace_members ⊆ RULES.member` and never
/// the converse. A row whose member does not exist yet is legal and wanted — that is how
/// `dudes-hook` is governed before it is written — and the converse is `--strict` (see
/// [`verdict`]), a mode rather than an assertion for exactly that reason. Do not "tighten" this
/// into an equality: it would turn today's tree red for a row that is doing its job.
pub(crate) fn uncovered_members(graph: &Graph) -> Result<Vec<String>> {
    let covered: BTreeSet<&str> = RULES.iter().map(|rule| rule.member).collect();
    Ok(graph
        .member_names()?
        .into_iter()
        .filter(|name| !covered.contains(name))
        .map(str::to_owned)
        .collect())
}

/// Run every rule against the real workspace and fail on any violation. With `strict`, a rule whose
/// member crate is missing also fails.
pub(crate) fn run(strict: bool) -> Result<()> {
    let metadata = metadata::cargo_metadata()?;
    let graph = Graph::from_value(&metadata)?;
    evaluate(&graph, strict)?;
    outbound::check(&metadata, &graph)
}

/// Evaluate the whole table against `graph`: every rule, then member coverage, then the verdict.
///
/// Split from [`run`] so the gate's end-to-end behaviour — a skip reaching the verdict, a member
/// with no row failing the run — is drivable from a test over a fixture graph rather than only
/// against whatever crates happen to exist today.
pub(crate) fn evaluate(graph: &Graph, strict: bool) -> Result<()> {
    let mut violations = 0usize;
    let mut skipped = 0usize;
    for rule in RULES {
        match check(graph, rule)? {
            Outcome::Skipped => {
                skipped += 1;
                println!(
                    "layering: {:<18} skipped (not in workspace yet)",
                    rule.member
                );
            }
            Outcome::Ok => println!("layering: {:<18} ok", rule.member),
            Outcome::Violated(paths) => {
                violations += paths.len();
                for path in paths {
                    let why = if outbound::BINARIES.contains(&rule.member)
                        && path.last().is_some_and(|target| {
                            outbound::FORBIDDEN_DEPS.contains(&target.as_str())
                        }) {
                        "shipped binaries link no outbound network clients"
                    } else {
                        rule.why
                    };
                    println!(
                        "layering: {:<18} VIOLATION {} ({})",
                        rule.member,
                        path.join(" -> "),
                        why
                    );
                }
            }
        }
    }
    let uncovered = uncovered_members(graph)?;
    for member in &uncovered {
        println!("layering: {member:<18} UNCOVERED (workspace member with no row in RULES)");
    }
    verdict(violations, uncovered.len(), skipped, strict)
}

/// Turn the counts into the exit status: violations and uncovered members always fail; skips fail
/// only under `--strict`.
fn verdict(violations: usize, uncovered: usize, skipped: usize, strict: bool) -> Result<()> {
    if violations > 0 {
        bail!("{violations} layering violation(s)");
    }
    if uncovered > 0 {
        bail!("{uncovered} workspace member(s) with no row in RULES; every member needs a row");
    }
    if strict && skipped > 0 {
        bail!("{skipped} layering rule(s) skipped; --strict requires every member crate to exist");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::{Value, json};

    use super::*;

    /// `(dependency, dep_kind)` — `None` is a normal dependency, `Some("dev")` a dev-dependency.
    type Dep<'a> = (&'a str, Option<&'a str>);

    /// `(dependency, dep_kinds)` — one edge carrying every kind cargo resolved for it, for the
    /// edges [`Dep`] cannot spell. Cargo emits a multi-entry `dep_kinds` when the same crate is
    /// both a normal and a dev dependency of a package, and four such edges resolve in this
    /// workspace today, so it is the shipped shape rather than a hypothetical one.
    type MixedDep<'a> = (&'a str, &'a [Option<&'a str>]);

    /// A package with no outgoing edges.
    const NO_DEPS: &[Dep<'static>] = &[];

    /// [`NO_DEPS`] for a [`graph_of_mixed`] fixture.
    const NO_MIXED_DEPS: &[MixedDep<'static>] = &[];

    /// The single construction site of one `deps` entry, so the `dep_kinds` shape is written once
    /// however many kinds an edge carries.
    fn dep_json(dep: &str, kinds: &[Option<&str>]) -> Value {
        let kinds: Vec<_> = kinds.iter().map(|kind| json!({ "kind": kind })).collect();
        json!({ "pkg": dep, "dep_kinds": kinds })
    }

    /// The single construction site of the `cargo metadata` envelope: `members` are the workspace
    /// members, `rows` the resolved `(package, deps)` nodes with their edges already built by
    /// [`dep_json`]. Package ids equal package names.
    fn graph_of_rows(members: &[&str], rows: &[(&str, Vec<Value>)]) -> Graph {
        let packages: Vec<_> = rows
            .iter()
            .map(|(name, _)| json!({ "id": name, "name": name }))
            .collect();
        let nodes: Vec<_> = rows
            .iter()
            .map(|(name, deps)| json!({ "id": name, "deps": deps }))
            .collect();
        let value = json!({
            "packages": packages,
            "resolve": { "nodes": nodes },
            "workspace_members": members,
        });
        Graph::from_value(&value).unwrap()
    }

    /// Build a fixture graph whose every edge carries exactly one dependency kind.
    fn graph(members: &[&str], rows: &[(&str, &[Dep<'_>])]) -> Graph {
        let rows: Vec<(&str, Vec<Value>)> = rows
            .iter()
            .map(|(name, deps)| {
                let deps = deps
                    .iter()
                    .map(|(dep, kind)| dep_json(dep, std::slice::from_ref(kind)))
                    .collect();
                (*name, deps)
            })
            .collect();
        graph_of_rows(members, &rows)
    }

    /// Build a fixture graph whose edges may carry several dependency kinds at once.
    fn graph_of_mixed(members: &[&str], rows: &[(&str, &[MixedDep<'_>])]) -> Graph {
        let rows: Vec<(&str, Vec<Value>)> = rows
            .iter()
            .map(|(name, deps)| {
                let deps = deps
                    .iter()
                    .map(|(dep, kinds)| dep_json(dep, kinds))
                    .collect();
                (*name, deps)
            })
            .collect();
        graph_of_rows(members, &rows)
    }

    /// Every [`RULES`] member as a workspace member with no dependencies, except those in `absent`
    /// — still packages in the graph, but no longer members of the workspace, which is exactly
    /// what a rule for a crate that has not been written yet sees.
    fn graph_of_every_member(absent: &[&str]) -> Graph {
        let members: Vec<&str> = RULES
            .iter()
            .map(|rule| rule.member)
            .filter(|member| !absent.contains(member))
            .collect();
        let rows: Vec<(&str, &[Dep<'_>])> =
            RULES.iter().map(|rule| (rule.member, NO_DEPS)).collect();
        graph(&members, &rows)
    }

    /// Look a rule up by its member crate, so adding a row to `RULES` touches nothing here.
    fn rule(member: &str) -> &'static Rule {
        RULES
            .iter()
            .find(|r| r.member == member)
            .unwrap_or_else(|| panic!("no layering rule for member `{member}`"))
    }

    /// The expected [`Outcome`] for a set of reported paths, each `member → … → forbidden`, in the
    /// order the row lists `forbidden`. The single construction site of `Outcome::Violated` in the
    /// tests, so the shape of a violation is written once.
    fn violated(paths: &[&[&str]]) -> Outcome {
        Outcome::Violated(
            paths
                .iter()
                .map(|path| path.iter().map(|hop| (*hop).to_owned()).collect())
                .collect(),
        )
    }

    /// `member` depending directly on `offender` is one violation on a one-hop path.
    ///
    /// [`every_pinned_offender_is_a_violation`] drives every entry of the table through this one shape.
    fn assert_direct_edge_is_a_violation(member: &str, offender: &str) {
        let graph = graph(
            &[member, offender],
            &[(member, &[(offender, None)]), (offender, NO_DEPS)],
        );
        assert_eq!(
            check(&graph, rule(member)).unwrap(),
            violated(&[&[member, offender]]),
            "`{offender}` must be forbidden to `{member}`"
        );
    }

    /// Every entry of every row in [`RULES`], written out independently of [`RULES`].
    ///
    /// **This duplication is the mechanism, not an oversight.** A test that reads
    /// `rule(m).forbidden` back and loops over it pins nothing: delete an entry and the loop simply
    /// gets shorter, still green. Because this table is written by hand and asserted **set-equal
    /// to `RULES` in both directions** by [`rules_match_the_pinned_table`], adding an entry is
    /// exactly as red as removing one — so a row can no longer grow into permitting the very thing
    /// it exists to forbid.
    ///
    /// **And it is a tripwire, not a rule stated twice.** Restating a rule risks *drift*: two copies of one fact, one of them edited, the other left behind and
    /// still read as true. Neither half of that is reachable here, for a mechanical reason rather
    /// than a careful one. [`rules_match_the_pinned_table`] asserts the two tables set-equal per
    /// member **and** over the member set, so every way they could disagree — an entry added to
    /// one, an entry removed from one, a row added to one, a row removed from one — is a failing
    /// test rather than a silent difference. There is no state in which one table is stale and the
    /// suite is green: they are in step, or they are red. A copy that cannot go stale carries none
    /// of the risk the rule exists to stop, and deleting either table would not remove a duplicate
    /// — it would remove the only check that either of them says what it claims to say.
    ///
    /// Adding a row to [`RULES`] means adding it here. Keep both in the same order.
    const PINNED: &[(&str, &[&str])] = &[
        // Vocabulary: no I/O, no clock, no runtime — and no entropy, because `ulid/std` is
        // `dep:rand` and this crate's `std` feature is where forwarding it would be decided.
        (
            "dudes-types",
            &[
                "tokio",
                "async-std",
                "smol",
                "async-io",
                "tracing",
                "redb",
                "rand",
                "getrandom",
            ],
        ),
        // The contract: it depends on nothing that acts.
        (
            "dudes-proto",
            &[
                "tokio",
                "async-std",
                "smol",
                "async-io",
                "tracing-subscriber",
                "redb",
                "dudes-journal",
                "dudes-policy",
                "dudes-supervisor",
                "dudes-pty",
                "dudes-proxy",
            ],
        ),
        // The synchronous writer: the contract and a store, nothing that acts.
        (
            "dudes-journal",
            &[
                "tokio",
                "async-std",
                "smol",
                "async-io",
                "dudes-policy",
                "dudes-supervisor",
                "dudes-pty",
                "dudes-proxy",
                "dudes-rpc",
                "dudes-providers",
            ],
        ),
        // Invariant 5: the loop is pure — and knows no vendor and no peer.
        (
            "dudes-policy",
            &[
                "tokio",
                "async-std",
                "smol",
                "async-io",
                "tracing",
                "tracing-subscriber",
                "redb",
                "dudes-journal",
                "dudes-supervisor",
                "dudes-pty",
                "dudes-proxy",
                "dudes-providers",
                "dudes-rpc",
                "rand",
                "getrandom",
            ],
        ),
        // An adapter builds specs and parses lines; it never acts.
        (
            "dudes-providers",
            &[
                "tokio",
                "async-std",
                "smol",
                "async-io",
                "tracing-subscriber",
                "rustls",
                "native-tls",
                "reqwest",
                "redb",
                "dudes-journal",
                "dudes-policy",
                "dudes-supervisor",
                "dudes-pty",
                "dudes-proxy",
                "dudes-rpc",
            ],
        ),
        // Invariant 9's second clause reaching one crate earlier than the shim: the shim's
        // transport is this crate, so a runtime here is a runtime there. Plus the door's own rule —
        // it decides nothing, carries no session bytes and owns no child. Its only
        // admitted edge is the Windows one to the DACL/filesystem leaf.
        (
            "dudes-rpc",
            &[
                "tokio",
                "async-std",
                "smol",
                "async-io",
                "rustls",
                "native-tls",
                "openssl-sys",
                "tracing-subscriber",
                "dudes-policy",
                "dudes-pty",
                "dudes-proxy",
                "dudes-host",
            ],
        ),
        // Invariant 9's first clause, plus the other half of the same row: `handle()` is sans-io,
        // so the supervisor reaches no journal, no store and no runtime either.
        (
            "dudes-supervisor",
            &[
                "dudes-pty",
                "dudes-proxy",
                "dudes-journal",
                "redb",
                "tokio",
                // The real host implements this crate's trait: the edge runs host -> supervisor and
                // never back.
                "dudes-host",
            ],
        ),
        // The host owns children and nothing else. The runtime names are the load-bearing ones:
        // tokio's process module reaps what it spawned, on its own schedule, which would free a
        // child's identifier mid-grace behind the host's back and dissolve the reservation the
        // whole kill path rests on. The crate that owns children cannot link the runtime that
        // would reap them. `dudes-policy` is absent on purpose — see the row in `RULES`.
        (
            "dudes-host",
            &[
                "tokio",
                "async-std",
                "smol",
                "async-io",
                "dudes-journal",
                "redb",
                "dudes-pty",
                "dudes-proxy",
                "dudes-rpc",
                "rustls",
                "native-tls",
                "reqwest",
            ],
        ),
        // The data-plane PTY host owns process trees and relays opaque bounded bytes. It reaches
        // typed launch/contract vocabulary plus native platform facilities, and nothing that
        // decides, journals, routes, serves a runtime or parses a terminal stream.
        (
            "dudes-pty",
            &[
                "tokio",
                "async-std",
                "smol",
                "async-io",
                "tracing-subscriber",
                "rustls",
                "native-tls",
                "openssl-sys",
                "reqwest",
                "redb",
                "dudes-journal",
                "dudes-policy",
                "dudes-supervisor",
                "dudes-rpc",
                "dudes-host",
                "dudes-win",
                "dudes-proxy",
                "alacritty_terminal",
                "termwiz",
                "vtparse",
                "wezterm-term",
            ],
        ),
        // The DACL/filesystem leaf carries one narrow platform capability. It reaches no other
        // workspace member and no process owner, loop, store, runtime or network dependency.
        (
            "dudes-win",
            &[
                "tokio",
                "async-std",
                "smol",
                "async-io",
                "tracing",
                "tracing-subscriber",
                "rustls",
                "native-tls",
                "openssl-sys",
                "reqwest",
                "redb",
                "dudes-types",
                "dudes-proto",
                "dudes-journal",
                "dudes-policy",
                "dudes-providers",
                "dudes-rpc",
                "dudes-supervisor",
                "dudes-host",
                "dudes-pty",
                "dudes-proxy",
                "dudes",
                "duded",
            ],
        ),
        // The daemon: it owns children through `dudes-host`, so it must not link a runtime that
        // would reap them; it terminates the control plane, so no TLS and no HTTP
        // client. It is also the composition root for the PTY leaf; the proxy remains elsewhere.
        (
            "duded",
            &[
                "tokio",
                "async-std",
                "smol",
                "async-io",
                "rustls",
                "native-tls",
                "openssl-sys",
                "reqwest",
                "dudes-proxy",
            ],
        ),
        // The CLI: a client. Synchronous like the transport it links, local like the socket it
        // reaches, and holding none of the four crates that would let it journal, decide, own a
        // child or fold a projection in the caller's process.
        (
            "dudes",
            &[
                "tokio",
                "async-std",
                "smol",
                "async-io",
                "rustls",
                "native-tls",
                "openssl-sys",
                "reqwest",
                "dudes-journal",
                "dudes-policy",
                "dudes-supervisor",
                "dudes-host",
                "dudes-pty",
                "dudes-proxy",
            ],
        ),
        // Invariant 9's second clause: no async runtime — which is not the same as "no tokio" —
        // no TLS, no subscriber.
        (
            "dudes-hook",
            &[
                "tokio",
                "async-std",
                "smol",
                "async-io",
                "rustls",
                "native-tls",
                "openssl-sys",
                "tracing-subscriber",
            ],
        ),
        // Repo automation: synchronous and offline.
        (
            "xtask",
            &[
                "tokio",
                "async-std",
                "smol",
                "async-io",
                "rustls",
                "native-tls",
                "openssl-sys",
                "reqwest",
            ],
        ),
    ];

    /// `member -> the crates forbidden to it`, rejecting a row that lists the same crate twice (a
    /// set comparison would not see it, and `check()` would report the same path twice and
    /// double-count one violation) or two rows for the same member.
    fn by_member(
        rows: impl Iterator<Item = (&'static str, &'static [&'static str])>,
        table: &str,
    ) -> BTreeMap<&'static str, BTreeSet<&'static str>> {
        let mut out = BTreeMap::new();
        for (member, forbidden) in rows {
            let set: BTreeSet<&str> = forbidden.iter().copied().collect();
            assert_eq!(
                set.len(),
                forbidden.len(),
                "{table}: `{member}` lists a crate twice: {forbidden:?}"
            );
            assert!(
                out.insert(member, set).is_none(),
                "{table}: two rows for `{member}`"
            );
        }
        out
    }

    /// The crates in `a` that are not in `b`.
    fn only_in<'a>(a: &BTreeSet<&'a str>, b: &BTreeSet<&'a str>) -> Vec<&'a str> {
        a.difference(b).copied().collect()
    }

    /// Invariant 9: the table says what it is asserted to say, **in both directions**.
    ///
    /// This is the check the gate was missing. Removing an entry from a row already turned some
    /// tests red; *adding* one changed nothing, so a row could quietly grow into permitting what it
    /// exists to forbid — and the two clauses invariant 9 actually names were deletable with the
    /// suite green. Set-equality against [`PINNED`], per member and over the member set, makes both
    /// directions fail.
    #[test]
    fn rules_match_the_pinned_table() {
        let rules = by_member(RULES.iter().map(|r| (r.member, r.forbidden)), "RULES");
        let pinned = by_member(PINNED.iter().copied(), "PINNED");

        let rule_members: BTreeSet<&str> = rules.keys().copied().collect();
        let pinned_members: BTreeSet<&str> = pinned.keys().copied().collect();
        assert_eq!(
            rule_members,
            pinned_members,
            "RULES and PINNED disagree on which members have a row: \
             in RULES only {:?}, in PINNED only {:?}",
            only_in(&rule_members, &pinned_members),
            only_in(&pinned_members, &rule_members),
        );

        for (member, forbidden) in &rules {
            let pinned_for = &pinned[member];
            assert_eq!(
                forbidden,
                pinned_for,
                "`{member}`'s row and its pinning table disagree: \
                 added to RULES but not pinned {:?}, pinned but missing from RULES {:?}",
                only_in(forbidden, pinned_for),
                only_in(pinned_for, forbidden),
            );
        }
    }

    /// Invariant 9: every pinned crate is a *violation*, not just a string in a table.
    ///
    /// Driven from [`PINNED`] rather than from [`RULES`] on purpose: paired with
    /// [`rules_match_the_pinned_table`] it means a crate cannot be pinned as text while being inert
    /// as a rule, and it cannot be dropped from the rule while staying pinned as text.
    #[test]
    fn every_pinned_offender_is_a_violation() {
        for (member, forbidden) in PINNED {
            for offender in *forbidden {
                assert_direct_edge_is_a_violation(member, offender);
            }
        }
    }

    #[test]
    fn shipped_network_bans_are_pinned_and_enforced_transitively() {
        let binaries = ["duded", "dudes", "dudes-hook"];
        let clients = [
            "async-net",
            "awc",
            "curl",
            "curl-sys",
            "hickory-resolver",
            "hyper",
            "hyper-util",
            "isahc",
            "native-tls",
            "openssl",
            "openssl-sys",
            "reqwest",
            "rustls",
            "socket2",
            "surf",
            "trust-dns-resolver",
            "tungstenite",
            "ureq",
        ];
        assert_eq!(outbound::BINARIES, binaries);
        assert_eq!(outbound::FORBIDDEN_DEPS, clients);
        for binary in binaries {
            for client in clients {
                assert_direct_edge_is_a_violation(binary, client);
                let graph = graph(
                    &[binary],
                    &[
                        (binary, &[("middle", None)]),
                        ("middle", &[(client, None)]),
                        (client, NO_DEPS),
                    ],
                );
                assert_eq!(
                    check(&graph, rule(binary)).unwrap(),
                    violated(&[&[binary, "middle", client]])
                );
            }
        }
    }

    #[test]
    fn hook_reqwest_dev_only_is_allowed_but_mixed_edge_fails() {
        let clean = graph(
            &["dudes-hook"],
            &[
                ("dudes-hook", &[("reqwest", Some("dev"))]),
                ("reqwest", NO_DEPS),
            ],
        );
        assert_eq!(check(&clean, rule("dudes-hook")).unwrap(), Outcome::Ok);
        let mixed = graph_of_mixed(
            &["dudes-hook"],
            &[
                ("dudes-hook", &[("reqwest", &[None, Some("dev")])]),
                ("reqwest", NO_MIXED_DEPS),
            ],
        );
        assert_eq!(
            check(&mixed, rule("dudes-hook")).unwrap(),
            violated(&[&["dudes-hook", "reqwest"]])
        );
    }

    /// Invariant 9: every row in `RULES` names a distinct member, so `run()` checks each member
    /// exactly once and a lookup by member binds to the only row for it. A duplicate row would be
    /// checked twice by the gate, and `rule()` would silently return whichever row came first —
    /// a test that reads as specific to a crate while asserting against another row's `forbidden`.
    #[test]
    fn members_are_unique() {
        let members: Vec<&str> = RULES.iter().map(|r| r.member).collect();
        let unique: BTreeSet<&str> = members.iter().copied().collect();
        assert_eq!(
            unique.len(),
            members.len(),
            "duplicate member in RULES: {members:?}"
        );
    }

    /// A new crate must add its row: a workspace member with no row in `RULES` is unenforced and
    /// must fail the run.
    #[test]
    fn a_workspace_member_with_no_row_is_uncovered_and_fails_the_run() {
        let covered = RULES[0].member;

        // Positive control: a workspace whose every member has a row reports nothing uncovered,
        // so the check below can distinguish "no member is uncovered" from "nothing was looked at".
        let clean = graph(&[covered], &[(covered, NO_DEPS)]);
        assert!(
            uncovered_members(&clean).unwrap().is_empty(),
            "`{covered}` has a row in RULES"
        );
        assert!(evaluate(&clean, false).is_ok());

        // The same workspace, plus a crate nobody wrote a row for.
        let dirty = graph(
            &[covered, "dudes-newcomer"],
            &[(covered, NO_DEPS), ("dudes-newcomer", NO_DEPS)],
        );
        assert_eq!(
            uncovered_members(&dirty).unwrap(),
            vec!["dudes-newcomer".to_owned()]
        );
        let err = evaluate(&dirty, false).unwrap_err().to_string();
        assert!(err.contains("no row in RULES"), "{err}");
    }

    /// A workspace member id with no resolve node is corrupt metadata. Coverage fails closed rather
    /// than dropping the member it could not name: dropping it would report that member as covered
    /// by a rule the gate never looked for.
    #[test]
    fn a_member_without_a_resolve_node_fails_closed() {
        let mut graph = graph(&["dudes-types"], &[("dudes-types", NO_DEPS)]);
        graph.members.push("ghost".to_owned());
        let err = uncovered_members(&graph).unwrap_err().to_string();
        assert!(err.contains("`ghost`"), "{err}");
    }

    /// `dudes-types` is vocabulary and `dudes-proto` is the contract: neither may reach a
    /// runtime, a store or a crate that acts. A dev-only edge does not count.
    #[test]
    fn types_and_proto_reach_nothing_that_acts() {
        let clean = graph(
            &["dudes-types", "dudes-proto"],
            &[
                ("dudes-types", &[("serde", None), ("tokio", Some("dev"))]),
                (
                    "dudes-proto",
                    &[("dudes-types", None), ("serde_json", None)],
                ),
                ("serde", NO_DEPS),
                ("serde_json", NO_DEPS),
                ("tokio", NO_DEPS),
            ],
        );
        assert_eq!(check(&clean, rule("dudes-types")).unwrap(), Outcome::Ok);
        assert_eq!(check(&clean, rule("dudes-proto")).unwrap(), Outcome::Ok);

        let dirty = graph(
            &["dudes-types", "dudes-proto"],
            &[
                ("dudes-types", &[("tokio", None)]),
                ("dudes-proto", &[("dudes-journal", None)]),
                ("dudes-journal", &[("redb", None)]),
                ("tokio", NO_DEPS),
                ("redb", NO_DEPS),
            ],
        );
        assert_eq!(
            check(&dirty, rule("dudes-types")).unwrap(),
            violated(&[&["dudes-types", "tokio"]])
        );
        assert_eq!(
            check(&dirty, rule("dudes-proto")).unwrap(),
            violated(&[
                &["dudes-proto", "dudes-journal", "redb"],
                &["dudes-proto", "dudes-journal"],
            ])
        );
    }

    /// `dudes-journal` is the synchronous writer: it may reach redb and the contract, never a
    /// runtime or a crate that acts. A dev-only edge does not count.
    #[test]
    fn journal_reaches_nothing_that_acts() {
        let clean = graph(
            &["dudes-proto", "dudes-journal"],
            &[
                ("dudes-proto", NO_DEPS),
                (
                    "dudes-journal",
                    &[
                        ("dudes-proto", None),
                        ("redb", None),
                        ("tokio", Some("dev")),
                    ],
                ),
                ("redb", NO_DEPS),
                ("tokio", NO_DEPS),
            ],
        );
        assert_eq!(check(&clean, rule("dudes-journal")).unwrap(), Outcome::Ok);
        let dirty = graph(
            &["dudes-journal"],
            &[
                ("dudes-journal", &[("dudes-supervisor", None)]),
                ("dudes-supervisor", &[("tokio", None)]),
                ("tokio", NO_DEPS),
            ],
        );
        assert_eq!(
            check(&dirty, rule("dudes-journal")).unwrap(),
            violated(&[
                &["dudes-journal", "dudes-supervisor", "tokio"],
                &["dudes-journal", "dudes-supervisor"],
            ])
        );
    }

    /// Invariant 5: `dudes-policy` is pure — it may reach the vocabulary and the contract, and a
    /// dev-only `proptest` edge does not count, but a runtime, a subscriber, a store, a random
    /// source or any crate that acts is a violation at any depth.
    ///
    /// The `rand`/`getrandom` entries need both arms to mean anything, because the crate the row
    /// forbids is the crate its own property tests are built on. `clean` puts `rand` and
    /// `getrandom` *behind* the dev-only `proptest` edge, so it fails if the prune drops that edge
    /// without the subtree behind it; `entropy` puts the same two crates on the normal path that was
    /// deleted — `dudes-policy → dudes-types → ulid → rand → getrandom`, the graph as it
    /// actually resolved — so it fails if the entries are dropped from the row. Either
    /// arm alone is satisfiable by a rule that does not discriminate.
    ///
    /// The prune's *depth* is not pinned here and `clean` cannot pin it: the `proptest` edge is
    /// declared on `dudes-policy` itself, the node the walk starts from, so a prune reaching only
    /// the starting node's edges still drops it and this test stays green.
    /// [`a_dev_edge_below_the_first_hop_is_pruned_with_its_subtree`] holds that half and is not
    /// redundant with this one — measured, not reasoned: under a prune limited to the starting
    /// node's edges this test passes and that one fails; under a prune that splices a pruned node's
    /// deps into its parent this test fails, at the `clean` arm.
    #[test]
    fn policy_reaches_nothing_that_acts() {
        let clean = graph(
            &["dudes-policy", "dudes-proto", "dudes-types"],
            &[
                (
                    "dudes-policy",
                    &[
                        ("dudes-proto", None),
                        ("dudes-types", None),
                        ("proptest", Some("dev")),
                    ],
                ),
                ("dudes-proto", &[("dudes-types", None)]),
                ("dudes-types", NO_DEPS),
                ("proptest", &[("rand", None)]),
                ("rand", &[("getrandom", None)]),
                ("getrandom", NO_DEPS),
            ],
        );
        assert_eq!(check(&clean, rule("dudes-policy")).unwrap(), Outcome::Ok);

        let dirty = graph(
            &["dudes-policy", "dudes-journal"],
            &[
                (
                    "dudes-policy",
                    &[("dudes-journal", None), ("tracing", None)],
                ),
                ("dudes-journal", &[("redb", None)]),
                ("tracing", NO_DEPS),
                ("redb", NO_DEPS),
            ],
        );
        assert_eq!(
            check(&dirty, rule("dudes-policy")).unwrap(),
            violated(&[
                &["dudes-policy", "tracing"],
                &["dudes-policy", "dudes-journal", "redb"],
                &["dudes-policy", "dudes-journal"],
            ])
        );

        let entropy = graph(
            &["dudes-policy", "dudes-types"],
            &[
                ("dudes-policy", &[("dudes-types", None)]),
                ("dudes-types", &[("ulid", None)]),
                ("ulid", &[("rand", None)]),
                ("rand", &[("getrandom", None)]),
                ("getrandom", NO_DEPS),
            ],
        );
        assert_eq!(
            check(&entropy, rule("dudes-policy")).unwrap(),
            violated(&[
                &["dudes-policy", "dudes-types", "ulid", "rand"],
                &["dudes-policy", "dudes-types", "ulid", "rand", "getrandom"],
            ])
        );
    }

    /// Invariant 5, the other half of the same row: the pure loop reaches no *adapter* and no
    /// *transport* either. Neither breaches invariant 5 mechanically — an adapter builds argv and a
    /// transport moves bytes, and `decide()` could call neither — but they are how the loop starts
    /// knowing which vendor it is deciding for and which peer asked. The edge compiles today and
    /// nothing caught it before this row named them.
    ///
    /// Reached through `dudes-supervisor`, a hop the row also forbids, so the fixture reports the
    /// transitive path as well and the assertion pins the whole reported set rather than one edge.
    #[test]
    fn policy_reaches_no_adapter_and_no_transport() {
        let graph = graph(
            &["dudes-policy", "dudes-supervisor"],
            &[
                ("dudes-policy", &[("dudes-supervisor", None)]),
                (
                    "dudes-supervisor",
                    &[("dudes-providers", None), ("dudes-rpc", None)],
                ),
                ("dudes-providers", NO_DEPS),
                ("dudes-rpc", NO_DEPS),
            ],
        );
        // Paths are reported in the order the row lists `forbidden`: supervisor, providers, rpc.
        assert_eq!(
            check(&graph, rule("dudes-policy")).unwrap(),
            violated(&[
                &["dudes-policy", "dudes-supervisor"],
                &["dudes-policy", "dudes-supervisor", "dudes-providers"],
                &["dudes-policy", "dudes-supervisor", "dudes-rpc"],
            ])
        );
    }

    /// Invariant 9: `dudes-providers` is an adapter — it builds launch specs and parses lines, so
    /// it may reach the vocabulary and the contract, and a dev-only edge does not count; a runtime,
    /// an HTTP or TLS stack, a store or a crate that acts is a violation at any depth.
    ///
    /// The intermediate hop is `vendor-sdk`, a crate no rule forbids, so every reported path is a
    /// transitive one and the assertion keeps distinguishing "dragged in through a hop" from
    /// "depended on directly" — the realistic way an adapter stops being pure.
    #[test]
    fn providers_reach_nothing_that_acts() {
        let clean = graph(
            &["dudes-providers", "dudes-proto", "dudes-types"],
            &[
                (
                    "dudes-providers",
                    &[
                        ("dudes-proto", None),
                        ("dudes-types", None),
                        ("serde_json", None),
                        ("tokio", Some("dev")),
                    ],
                ),
                ("dudes-proto", &[("dudes-types", None)]),
                ("dudes-types", NO_DEPS),
                ("serde_json", NO_DEPS),
                ("tokio", NO_DEPS),
            ],
        );
        assert_eq!(check(&clean, rule("dudes-providers")).unwrap(), Outcome::Ok);

        let dirty = graph(
            &["dudes-providers"],
            &[
                ("dudes-providers", &[("vendor-sdk", None)]),
                ("vendor-sdk", &[("reqwest", None)]),
                ("reqwest", &[("tokio", None), ("rustls", None)]),
                ("tokio", NO_DEPS),
                ("rustls", NO_DEPS),
            ],
        );
        // Paths are reported in the order the row lists `forbidden`: tokio, rustls, reqwest.
        assert_eq!(
            check(&dirty, rule("dudes-providers")).unwrap(),
            violated(&[
                &["dudes-providers", "vendor-sdk", "reqwest", "tokio"],
                &["dudes-providers", "vendor-sdk", "reqwest", "rustls"],
                &["dudes-providers", "vendor-sdk", "reqwest"],
            ])
        );
    }

    /// The one edge the host is *required* to have, and the reason two obvious names are missing
    /// from its row. `dudes-host` implements `dudes_supervisor::host::ProcessHost`, so it
    /// depends on `dudes-supervisor` and through it on `dudes-policy`. This gate reads transitive
    /// reachability, so forbidding either would be a permanent false alarm rather than a rule —
    /// which is also why `dudes-supervisor`'s own row omits `dudes-policy`.
    #[test]
    fn the_host_may_depend_on_the_supervisor_it_implements() {
        let graph = graph(
            &["dudes-host", "dudes-supervisor", "dudes-policy"],
            &[
                ("dudes-host", &[("dudes-supervisor", None)]),
                ("dudes-supervisor", &[("dudes-policy", None)]),
                ("dudes-policy", &[]),
            ],
        );
        assert_eq!(check(&graph, rule("dudes-host")).unwrap(), Outcome::Ok);
    }

    /// Admits one Windows capability edge from the transport to the DACL/filesystem leaf.
    /// The leaf may reach `windows-sys`, while the sibling child-owning crate remains forbidden.
    #[test]
    fn rpc_may_reach_the_windows_leaf_but_not_the_child_owner() {
        let admitted = graph(
            &["dudes-rpc", "dudes-win"],
            &[
                ("dudes-rpc", &[("dudes-win", None)]),
                ("dudes-win", &[("windows-sys", None)]),
                ("windows-sys", NO_DEPS),
            ],
        );
        assert_eq!(check(&admitted, rule("dudes-rpc")).unwrap(), Outcome::Ok);
        assert_eq!(check(&admitted, rule("dudes-win")).unwrap(), Outcome::Ok);

        let child_owner = graph(
            &["dudes-rpc", "dudes-host"],
            &[
                ("dudes-rpc", &[("dudes-host", None)]),
                ("dudes-host", NO_DEPS),
            ],
        );
        assert_eq!(
            check(&child_owner, rule("dudes-rpc")).unwrap(),
            violated(&[&["dudes-rpc", "dudes-host"]])
        );
    }

    /// Admits the child owner reaching the DACL/filesystem leaf for agent-owned paths;
    /// the reverse edge remains forbidden by the leaf's responsibility boundary.
    #[test]
    fn host_may_reach_the_windows_leaf_but_the_leaf_cannot_reach_the_host() {
        let admitted = graph(
            &["dudes-host", "dudes-win", "windows-sys"],
            &[
                ("dudes-host", &[("dudes-win", Some("cfg(windows)"))]),
                ("dudes-win", &[("windows-sys", Some("cfg(windows)"))]),
                ("windows-sys", NO_DEPS),
            ],
        );
        assert_eq!(check(&admitted, rule("dudes-host")).unwrap(), Outcome::Ok);
        assert_eq!(check(&admitted, rule("dudes-win")).unwrap(), Outcome::Ok);

        let reversed = graph(
            &["dudes-win", "dudes-host"],
            &[
                ("dudes-win", &[("dudes-host", Some("cfg(windows)"))]),
                ("dudes-host", NO_DEPS),
            ],
        );
        assert_eq!(
            check(&reversed, rule("dudes-win")).unwrap(),
            violated(&[&["dudes-win", "dudes-host"]])
        );
    }

    /// Invariant 9: a rule for a crate that does not exist yet passes as `Skipped`, so the gate is
    /// live on an empty workspace without blocking it. This pins `check()`'s per-rule answer;
    /// [`strict_fails_end_to_end_when_a_member_is_absent`] pins what the verdict does with it.
    #[test]
    fn missing_member_is_skipped() {
        let graph = graph(&["xtask"], &[("xtask", NO_DEPS)]);
        assert_eq!(
            check(&graph, rule("dudes-supervisor")).unwrap(),
            Outcome::Skipped
        );
        assert_eq!(check(&graph, rule("dudes-hook")).unwrap(), Outcome::Skipped);
    }

    /// Invariant 9: `--strict` turns a skipped rule into a failure, so the gate cannot stay green
    /// by a member crate silently going missing once every member is expected to exist.
    #[test]
    fn strict_fails_on_skipped_rules() {
        assert!(verdict(0, 0, 1, true).is_err());
        assert!(verdict(0, 0, 1, false).is_ok());
        assert!(verdict(0, 0, 0, true).is_ok());
        // A violation fails in both modes.
        assert!(verdict(1, 0, 0, false).is_err());
        assert!(verdict(1, 0, 0, true).is_err());
    }

    /// A workspace member with no row fails the gate in both modes: coverage is not a strictness
    /// setting. `--strict` is about rows whose crate does not exist yet; this is about crates that
    /// exist with no rule, which is unenforced code and never acceptable.
    #[test]
    fn uncovered_members_fail_the_verdict_in_both_modes() {
        assert!(verdict(0, 1, 0, false).is_err());
        assert!(verdict(0, 1, 0, true).is_err());
        assert!(verdict(0, 0, 0, false).is_ok());
    }

    /// Invariant 9, end to end: a skip has to survive rule evaluation and reach the verdict. Under
    /// `--strict` the run fails; without it, it passes.
    ///
    /// [`strict_fails_on_skipped_rules`] pins [`verdict`] on counts; this drives the whole of
    /// [`evaluate`] over a graph, so deleting the `strict && skipped > 0` branch turns it red even
    /// though every rule evaluated fine. The all-present fixture is the positive control: the same
    /// code path with nothing to skip stays green in both modes.
    #[test]
    fn strict_fails_end_to_end_when_a_member_is_absent() {
        let all_present = graph_of_every_member(&[]);
        assert!(evaluate(&all_present, false).is_ok());
        assert!(
            evaluate(&all_present, true).is_ok(),
            "every member present: --strict has nothing to fail on"
        );

        let absent = RULES[0].member;
        let missing_one = graph_of_every_member(&[absent]);
        assert!(
            evaluate(&missing_one, false).is_ok(),
            "without --strict a missing `{absent}` is a skip, not a failure"
        );
        let err = evaluate(&missing_one, true).unwrap_err().to_string();
        assert!(err.contains("skipped"), "{err}");
    }

    /// Invariant 9: a dependency edge whose target has no resolve node is corrupt metadata. The
    /// gate fails closed (an error) instead of reporting "not reachable" and passing.
    #[test]
    fn missing_resolve_node_fails_closed() {
        // `dudes-supervisor` -> `mystery`, but `mystery` has no node of its own.
        let graph = graph(
            &["dudes-supervisor"],
            &[("dudes-supervisor", &[("mystery", None)])],
        );
        let err = check(&graph, rule("dudes-supervisor")).unwrap_err();
        assert!(err.to_string().contains("`mystery`"), "{err}");
    }

    /// Invariant 9: `dudes-supervisor` reaching `dudes-pty` through any depth is a violation, and
    /// the reported path names every hop.
    ///
    /// The intermediate hop is `dudes-providers` — a crate the supervisor is *allowed* to depend
    /// on — so the only violation the fixture can produce is the transitive one this test is
    /// about. An intermediate that is itself forbidden would report two paths and the assertion
    /// would stop distinguishing "reached through a hop" from "depended on directly".
    #[test]
    fn supervisor_reaching_pty_transitively_is_a_violation() {
        let graph = graph(
            &["dudes-supervisor", "dudes-providers", "dudes-pty"],
            &[
                ("dudes-supervisor", &[("dudes-providers", None)]),
                ("dudes-providers", &[("dudes-pty", None)]),
                ("dudes-pty", NO_DEPS),
            ],
        );
        assert_eq!(
            check(&graph, rule("dudes-supervisor")).unwrap(),
            violated(&[&["dudes-supervisor", "dudes-providers", "dudes-pty"]])
        );
    }

    #[test]
    fn daemon_may_consume_the_pty_leaf_directly() {
        let graph = graph(
            &["duded", "dudes-pty"],
            &[("duded", &[("dudes-pty", None)]), ("dudes-pty", NO_DEPS)],
        );
        assert_eq!(check(&graph, rule("duded")).unwrap(), Outcome::Ok);
    }

    #[test]
    fn current_normal_pty_consumers_are_exactly_the_daemon() {
        let graph =
            Graph::from_value(&metadata::cargo_metadata().expect("current locked metadata"))
                .expect("current resolved graph");
        let pty = graph
            .nodes
            .iter()
            .find_map(|(id, node)| (node.name == "dudes-pty").then_some(id))
            .expect("dudes-pty resolve node");
        let consumers: BTreeSet<_> = graph
            .members
            .iter()
            .filter_map(|id| {
                let node = graph.nodes.get(id)?;
                node.deps.contains(pty).then_some(node.name.as_str())
            })
            .collect();
        assert_eq!(consumers, BTreeSet::from(["duded"]));
    }

    /// Invariant 9: `dudes-hook` may use tokio in tests only — a dev-only edge does not count,
    /// because it never links into the shipped binary.
    #[test]
    fn hook_dev_only_tokio_is_allowed() {
        let graph = graph(
            &["dudes-hook"],
            &[
                ("dudes-hook", &[("tokio", Some("dev")), ("serde", None)]),
                ("tokio", NO_DEPS),
                ("serde", NO_DEPS),
            ],
        );
        assert_eq!(check(&graph, rule("dudes-hook")).unwrap(), Outcome::Ok);
    }

    /// Invariant 9: `dudes-hook` linking tokio through a normal dependency is a violation.
    #[test]
    fn hook_normal_tokio_is_a_violation() {
        let graph = graph(
            &["dudes-hook"],
            &[
                ("dudes-hook", &[("rpc-client", None)]),
                ("rpc-client", &[("tokio", None)]),
                ("tokio", NO_DEPS),
            ],
        );
        assert_eq!(
            check(&graph, rule("dudes-hook")).unwrap(),
            violated(&[&["dudes-hook", "rpc-client", "tokio"]])
        );
    }

    /// Invariant 9: an edge cargo resolved as **both** a normal and a dev dependency links into the
    /// shipped binary, so the gate keeps it — and keeps the subtree behind it. A dev kind sitting
    /// alongside a normal one does not make the edge dev-only.
    ///
    /// This completes the trio: [`hook_dev_only_tokio_is_allowed`] pins the dev-only edge,
    /// [`hook_normal_tokio_is_a_violation`] the normal one, and nothing pinned the mixed edge.
    /// `dev_only` in [`crate::metadata`] decides it with `all` over `dep_kinds`, and weakening that
    /// `all` to `any` left the whole suite *and* `cargo xtask layering` green while the graph the
    /// gate walks silently lost `dudes -> dudes-rpc -> nix -> libc`. Four normal+dev edges resolve
    /// in this workspace today — `dudes -> dudes-rpc` among them — so the over-prune reaches the
    /// reasoning the `dudes` row states above: that dropping dev edges is what makes the row assert
    /// the *shipped* binary's graph.
    ///
    /// The two arms differ in exactly one thing, the kind list on the first hop, and both are
    /// load-bearing. The dev-only arm is the positive control that pruning happens at all, so a
    /// `dev_only` that never fired would redden it rather than pass by making the mixed arm true.
    #[test]
    fn hook_normal_and_dev_tokio_is_a_violation() {
        let dev_only = graph_of_mixed(
            &["dudes-hook"],
            &[
                ("dudes-hook", &[("rpc-client", &[Some("dev")])]),
                ("rpc-client", &[("tokio", &[None])]),
                ("tokio", NO_MIXED_DEPS),
            ],
        );
        assert_eq!(check(&dev_only, rule("dudes-hook")).unwrap(), Outcome::Ok);

        let mixed = graph_of_mixed(
            &["dudes-hook"],
            &[
                ("dudes-hook", &[("rpc-client", &[None, Some("dev")])]),
                ("rpc-client", &[("tokio", &[None])]),
                ("tokio", NO_MIXED_DEPS),
            ],
        );
        assert_eq!(
            check(&mixed, rule("dudes-hook")).unwrap(),
            violated(&[&["dudes-hook", "rpc-client", "tokio"]])
        );
    }

    /// Invariant 5, and the *reach* of the prune: a dev-only edge **below** the first hop is
    /// dropped as well, and the subtree behind it goes with it.
    ///
    /// This is the shipped shape, hop for hop:
    /// `dudes-policy -> dudes-proto --dev--> jsonschema -> getrandom` is how `getrandom` sits in
    /// the resolved graph today — the dev edge two hops from the member the rule is about, and the
    /// forbidden crate *behind* the pruned node rather than at the end of the pruned edge. Every
    /// other dev edge in this module is declared on the member of a rule its fixture is checked
    /// against, so the depth half of the property was pinned in exactly one place and only by
    /// accident: `types_and_proto_reach_nothing_that_acts` also checks `dudes-proto`'s row, where
    /// the `dudes-types --dev--> tokio` edge lands a hop down — which its doc does not mention and
    /// which would be lost the moment that fixture's crates changed. Here the depth is what is
    /// being asserted rather than a side effect of the shape.
    ///
    /// The two arms differ in exactly one thing, the kind on the `dudes-proto -> jsonschema` edge.
    /// `pruned` reddens if the prune is depth-limited to the starting node's edges, and if it
    /// drops the edge without the subtree behind it. `kept` is the positive control — the same
    /// graph with that one edge normal — and reddens if `getrandom` leaves the row, so neither arm
    /// is satisfiable by a rule that does not discriminate.
    #[test]
    fn a_dev_edge_below_the_first_hop_is_pruned_with_its_subtree() {
        let pruned = graph(
            &["dudes-policy", "dudes-proto"],
            &[
                ("dudes-policy", &[("dudes-proto", None)]),
                ("dudes-proto", &[("jsonschema", Some("dev"))]),
                ("jsonschema", &[("getrandom", None)]),
                ("getrandom", NO_DEPS),
            ],
        );
        assert_eq!(check(&pruned, rule("dudes-policy")).unwrap(), Outcome::Ok);

        let kept = graph(
            &["dudes-policy", "dudes-proto"],
            &[
                ("dudes-policy", &[("dudes-proto", None)]),
                ("dudes-proto", &[("jsonschema", None)]),
                ("jsonschema", &[("getrandom", None)]),
                ("getrandom", NO_DEPS),
            ],
        );
        assert_eq!(
            check(&kept, rule("dudes-policy")).unwrap(),
            violated(&[&["dudes-policy", "dudes-proto", "jsonschema", "getrandom"]])
        );
    }
}
